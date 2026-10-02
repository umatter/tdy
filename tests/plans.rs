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

fn switch_to_lock(t: &Path) {
    let sql = std::fs::read_to_string(t).unwrap();
    let with = sql.replace("date_order = 'dmy'\n);", "date_order = 'dmy',\n  plans      = 'lock'\n);");
    assert_ne!(with, sql);
    std::fs::write(t, with).unwrap();
}

/// Switching an existing pile to `plans = 'lock'` deletes nothing: a
/// tool-written sidecar left from before is reused as it always was.
/// `--prune-sidecars` is the one way a sidecar leaves — only when the plan
/// this fit proves is identical, and never a person's.
#[test]
fn prune_moves_identical_sidecars_and_keeps_a_hand_written_or_edited_one() {
    let (dir, t) = staged(false);
    fit(&t);
    assert_eq!(sidecars(dir.path()).len(), 9);
    switch_to_lock(&t);

    let text = fit(&t);
    assert_eq!(sidecars(dir.path()).len(), 9, "nothing is deleted without the flag");
    assert!(text.contains("plans: 0 member(s) share 0 plan(s), held in the lock"), "{text}");

    // A person's sidecar, and a tool-written one edited by hand.
    let feb = tdy::sidecar::sidecar_path(&dir.path().join("2025-02.csv"));
    let s = std::fs::read_to_string(&feb).unwrap();
    std::fs::write(&feb, s.replace("method = \"heuristic\"", "method = \"manual\"")).unwrap();
    let jan = tdy::sidecar::sidecar_path(&dir.path().join("2025-01.csv"));
    let s = std::fs::read_to_string(&jan).unwrap();
    let edited = s.replacen("    \"keine\",\n", "", 1);
    assert_ne!(edited, s);
    std::fs::write(&jan, &edited).unwrap();

    let out = tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars"]);
    let text = ok(&out);
    assert!(
        text.contains(
            "--prune-sidecars: 7 moved into the lock and removed, 1 kept (hand-written), 1 kept \
             (differs from the plan this fit proved)"
        ),
        "{text}"
    );
    assert_eq!(sidecars(dir.path()), vec!["2025-01.csv.tdy.toml", "2025-02.csv.tdy.toml"]);
    assert_eq!(std::fs::read_to_string(&jan).unwrap(), edited, "the edited one is untouched");
    let lock = lock_text(&t);
    assert_eq!(lock.matches("\nspec = \"b3:").count(), 7, "{lock}");
    let q = ok(&query(&t, "SELECT sum(amount) AS total FROM dataset('@')"));
    assert!(q.contains("57340.00"), "{q}");

    // Pruning again finds nothing more to move.
    let text = ok(&tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars"]));
    assert!(text.contains("--prune-sidecars: 0 moved"), "{text}");
}

/// A dry run says what pruning would do and removes nothing.
#[test]
fn a_dry_run_prune_removes_nothing() {
    let (dir, t) = staged(false);
    fit(&t);
    switch_to_lock(&t);
    let text = ok(&tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars", "--dry-run"]));
    assert!(text.contains("--prune-sidecars: 9 moved into the lock, none removed"), "{text}");
    assert_eq!(sidecars(dir.path()).len(), 9);
}

/// On a target that keeps its plans in sidecars there is nowhere to move
/// them: refused, saying why.
#[test]
fn prune_is_refused_on_a_sidecars_target() {
    let (dir, t) = staged(false);
    fit(&t);
    let out = tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("keeps its plans in sidecars") && err.contains("plans = 'lock'"), "{err}");
    assert_eq!(sidecars(dir.path()).len(), 9);
}

/// The console's `.fit … --prune-sidecars` is the same function.
#[tokio::test]
async fn the_console_prunes_through_the_same_function() {
    let (dir, t) = staged(false);
    fit(&t);
    switch_to_lock(&t);
    let cfg = tdy::config::load(&tdy::config::Overrides { backend: Some("none".into()), model: None, base_url: None })
        .unwrap();
    let mut s = tdy::console::Session::new(dir.path(), cfg).unwrap();
    let o = s.run(".fit sales_ok.tdy.sql --prune-sidecars", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains("--prune-sidecars: 9 moved into the lock and removed"), "{}", o.text);
    assert_eq!(sidecars(dir.path()), Vec::<String>::new());
}

fn no_llm() -> tdy::config::Config {
    tdy::config::load(&tdy::config::Overrides { backend: Some("none".into()), model: None, base_url: None }).unwrap()
}

/// `tdy check TARGET --against FILE` on a member with no sidecar checks the
/// plan the lock holds for it, and says that is where it came from.
#[test]
fn check_against_a_lock_held_member_checks_the_lock_plan() {
    let (dir, t) = staged(true);
    fit(&t);
    let jan = dir.path().join("2025-01.csv");
    let out = tdy(&["check", t.to_str().unwrap(), "--against", jan.to_str().unwrap()]);
    let text = ok(&out);
    assert!(text.contains("2025-01.csv: CONFORMS — plan held in "), "{text}");
    assert!(text.contains("sales_ok.tdy.lock (spec b3:"), "{text}");
    assert!(text.contains("1 of 1 file(s) conform to `sales_ok`."), "{text}");

    let out = tdy(&["--json", "check", t.to_str().unwrap(), "--against", jan.to_str().unwrap()]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["files"][0]["verdict"], "conforms", "{v}");
    assert_eq!(v["files"][0]["plan"], "lock", "{v}");

    // Changed since it was planned: not the plan a query would use.
    let mut body = std::fs::read(&jan).unwrap();
    body.extend_from_slice(b"31.01.2025;Mitte;1'000.00\n");
    std::fs::write(&jan, body).unwrap();
    let out = tdy(&["check", t.to_str().unwrap(), "--against", jan.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success());
    assert!(text.contains("2025-01.csv: STALE — the file has changed since its plan was recorded in "), "{text}");
}

/// `tdy validate FILE` on a member with no sidecar says its plan is held in
/// the target's lock — and checks that plan against the file.
#[test]
fn validate_on_a_lock_held_member_names_the_lock() {
    let (dir, t) = staged(true);
    fit(&t);
    let jan = dir.path().join("2025-01.csv");
    let text = ok(&tdy(&["validate", jan.to_str().unwrap()]));
    assert!(text.contains("2025-01.csv: ok"), "{text}");
    assert!(
        text.contains("note: no sidecar — its plan is held in the lock of ") && text.contains("sales_ok.tdy.sql (spec b3:"),
        "{text}"
    );

    let out = tdy(&["validate", jan.to_str().unwrap(), "--stamp"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "stamping a sidecar that does not exist");
    assert!(err.contains("has no sidecar to stamp: its plan is held in the lock of"), "{err}");
    assert!(err.contains("method = \"manual\""), "{err}");
}

/// `tdy profile` of a lock-held member reads it with the plan the lock
/// holds, and says so.
#[test]
fn profile_of_a_lock_held_member_reads_the_lock_plan() {
    let (dir, t) = staged(true);
    fit(&t);
    let jan = dir.path().join("2025-01.csv");
    let p = tdy::profile::profile_file(&jan, &tdy::profile::Request::default(), no_llm().limits).unwrap();
    assert_eq!(p.frame, "the lock of sales_ok.tdy.sql", "{}", p.frame);
    let text = ok(&tdy(&["profile", jan.to_str().unwrap()]));
    assert!(text.contains("frame: the lock of sales_ok.tdy.sql"), "{text}");
}

/// The console: `.ls` names a lock-held member's plan without a sidecar to
/// read, and `.accept` shows the evidence for a plan the lock holds.
#[tokio::test]
async fn the_console_lists_and_accepts_lock_held_members() {
    let (dir, t) = cents_pile();
    fit(&t);
    let mut s = tdy::console::Session::new(dir.path(), no_llm()).unwrap();
    let o = s.run(".ls", None).await;
    assert!(o.ok, "{}", o.text);
    let line = o.text.lines().find(|l| l.starts_with("2025-03.csv")).unwrap();
    assert!(line.ends_with("plan in the lock"), "{}", o.text);
    let entries = tdy::console::list_dir(dir.path()).unwrap();
    let march = entries.iter().find(|e| e.name == "2025-03.csv").unwrap();
    assert_eq!(march.status, tdy::console::EntryStatus::InLock);

    let o = s.run(".accept sales.tdy.sql 2025-03.csv", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.starts_with("evidence for 2025-03.csv (nothing written):\n"), "{}", o.text);
    assert!(o.text.contains("  plan: held in the lock (spec b3:"), "{}", o.text);
    let o = s.run(".accept sales.tdy.sql 2025-03.csv", None).await;
    assert!(o.ok && o.text.starts_with("accepted 2025-03.csv"), "{}", o.text);
    ok(&query(&t, "SELECT count(*) FROM dataset('@')"));

    // Changed bytes: `.ls` says stale, as it does for a stale sidecar.
    let p = dir.path().join("2025-01.csv");
    let mut body = std::fs::read(&p).unwrap();
    body.extend_from_slice(b"28.01.2025;Mitte;1.00\n");
    std::fs::write(&p, body).unwrap();
    let entries = tdy::console::list_dir(dir.path()).unwrap();
    assert_eq!(entries.iter().find(|e| e.name == "2025-01.csv").unwrap().status, tdy::console::EntryStatus::Stale);
}

/// `n` one-row CSVs that share one plan, and a target over them.
fn many(n: usize, plans: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    for i in 0..n {
        std::fs::write(dir.path().join(format!("m{i:04}.csv")), format!("Datum;Region;Betrag\n28.01.2025;Ost;{i}.00\n"))
            .unwrap();
    }
    let t = dir.path().join("many.tdy.sql");
    std::fs::write(
        &t,
        format!(
            "CREATE TABLE many (month DATE NOT NULL OPTIONS(matches='Datum'), region TEXT NOT NULL \
             OPTIONS(matches='Region'), amount DECIMAL(14,2) NOT NULL OPTIONS(matches='Betrag')) \
             WITH (files = '*.csv', date_order = 'dmy'{plans});"
        ),
    )
    .unwrap();
    (dir, t)
}

const HINT: &str = "Declaring plans = 'lock' in the target's WITH clause keeps it once, in the lock.";

/// A sidecar pile that just wrote 200 sidecars of one plan says once, on
/// stderr, that the option exists; the pile text a script reads is
/// untouched, and a smaller pile, a lock pile or a refit say nothing.
#[test]
fn a_fit_that_writes_200_sidecars_of_one_plan_names_the_option_once() {
    let (dir, t) = many(200, "");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = ok(&out);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.matches(HINT).count(), 1, "{err}");
    assert!(err.contains("note: 200 of the sidecars this fit wrote hold one plan."), "{err}");
    assert!(!text.contains("plans = 'lock'"), "never in the pile text:\n{text}");
    assert_eq!(sidecars(dir.path()).len(), 200);

    let again = tdy(&["fit", t.to_str().unwrap()]);
    assert!(!String::from_utf8_lossy(&again.stderr).contains(HINT), "a refit writes no sidecar");

    let (_d, t) = many(199, "");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains(HINT));

    let (d, t) = many(200, ", plans = 'lock'");
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = ok(&out);
    assert!(!String::from_utf8_lossy(&out.stderr).contains(HINT));
    assert!(text.contains("plans: 200 member(s) share 1 plan(s), held in the lock"), "{text}");
    assert_eq!(sidecars(d.path()), Vec::<String>::new());
}

/// `tdy draft` over a pile of 200 files or more declares `plans = 'lock'`,
/// with a comment line above the clause; the draft still parses. Under 200
/// it says nothing about plans.
#[test]
fn draft_declares_plans_in_the_lock_for_a_pile_of_200() {
    let (dir, _) = many(200, "");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "csv"))
        .collect();
    files.sort();
    let sql = tdy::draft::draft_target(&files, tdy::config::Limits::default()).unwrap();
    let target = tdy::target::Target::parse(&sql).unwrap_or_else(|e| panic!("{sql}\n{e:#}"));
    assert_eq!(target.plans, tdy::target::PlanStore::Lock, "{sql}");
    let with = sql.find("\nWITH (").unwrap();
    let above = sql[..with].lines().last().unwrap();
    assert!(above.starts_with("-- ") && above.contains("200 files"), "a comment line above the clause:\n{sql}");
    assert!(sql.contains("  plans = 'lock'"), "{sql}");

    let sql = tdy::draft::draft_target(&files[..199], tdy::config::Limits::default()).unwrap();
    assert!(!sql.contains("plans"), "{sql}");
    assert_eq!(tdy::target::Target::parse(&sql).unwrap().plans, tdy::target::PlanStore::Sidecars);
}

/// The drifting exports plus a file of three stacked blocks and a workbook
/// whose two sheets both fit, every block accepted: 51 rows, 62,440.00.
fn blocks_and_sheets() -> (TempDir, PathBuf) {
    let (dir, t) = staged(true);
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    std::fs::copy(data.join("regions_three.csv"), dir.path().join("2025-13.csv")).unwrap();
    std::fs::copy(data.join("sheet_frames_two_fit.xlsx"), dir.path().join("2025-14.xlsx")).unwrap();
    let ts = t.to_str().unwrap();
    fit(&t);
    ok(&tdy(&["fit", ts, "--accept", "2025-13.csv#1", "--accept", "2025-13.csv#2", "--accept", "2025-13.csv#3"]));
    let q = ok(&query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')"));
    assert!(q.contains(" 51 ") && q.contains("62440.00"), "{q}");
    (dir, t)
}

/// Point member `a` at the plan member `b` names — a one-line edit of the lock.
fn repoint(t: &Path, a: (&str, Option<&str>, Option<u32>), b: (&str, Option<&str>, Option<u32>)) {
    let mut lock = tdy::lockfile::Lock::load(t).unwrap().unwrap();
    let id = lock.member(b.0, b.1, b.2).unwrap().spec.clone().unwrap();
    let m = lock.members.iter_mut().find(|m| m.path == a.0 && m.sheet.as_deref() == a.1 && m.region == a.2).unwrap();
    assert_ne!(m.spec.as_deref(), Some(id.as_str()), "the two already share a plan; pick another pair");
    m.spec = Some(id);
    lock.save(t).unwrap();
}

/// A lock-held plan is about one sheet or one block. Handed to a member it
/// does not read — two `spec =` lines swapped — it is refused by name, as a
/// sidecar about the wrong sheet or block always was, and a refit re-plans it.
#[test]
fn a_lock_plan_for_another_block_or_sheet_is_refused() {
    for (a, b, said) in [
        (("2025-13.csv", None, Some(1)), ("2025-13.csv", None, Some(3)), "region 1"),
        (("2025-14.xlsx", Some("Q1"), None), ("2025-14.xlsx", Some("Q2"), None), "sheet \"Q1\""),
    ] {
        let (dir, t) = blocks_and_sheets();
        repoint(&t, a, b);
        let out = query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{said}: a plan for another member was served:\n{}", String::from_utf8_lossy(&out.stdout));
        assert!(err.contains(said) && err.contains("plan"), "{said}: {err}");

        let member = match (a.1, a.2) {
            (Some(s), _) => format!("2025-14.xlsx#{s}"),
            (_, Some(r)) => format!("2025-13.csv#{r}"),
            _ => unreachable!(),
        };
        let out = tdy(&["check", t.to_str().unwrap(), "--against", dir.path().join(&member).to_str().unwrap()]);
        assert!(!String::from_utf8_lossy(&out.stdout).contains("CONFORMS"), "{said}: {}", String::from_utf8_lossy(&out.stdout));
        assert!(!out.status.success());

        let text = fit(&t);
        assert!(text.contains("plan refused"), "{said}: {text}");
        // The acceptance was given to the plan the lock named; the plan the
        // refit proved is another, so it is asked for again.
        if a.2.is_some() {
            assert!(text.contains("REVIEW"), "{said}: {text}");
            ok(&tdy(&["fit", t.to_str().unwrap(), "--accept", &member]));
        }
        let q = ok(&query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')"));
        assert!(q.contains(" 51 ") && q.contains("62440.00"), "{said}: {q}");
    }
}

/// Re-record the plan `member` names as framed by a model: what a lock
/// written by a fit with a backend holds, id and all.
fn as_model_framed(t: &Path, member: &str) -> String {
    let mut lock = tdy::lockfile::Lock::load(t).unwrap().unwrap();
    let old = lock.member(member, None, None).unwrap().spec.clone().unwrap();
    let e = lock.specs.iter_mut().find(|e| e.id == old).unwrap();
    e.method = tdy::spec::InferenceMethod::Llm;
    e.model = Some("x".into());
    let new = tdy::plans::spec_id(&e.spec, e.method, e.model.as_deref()).unwrap();
    e.id = new.clone();
    for m in lock.members.iter_mut().filter(|m| m.spec.as_deref() == Some(old.as_str())) {
        m.spec = Some(new.clone());
    }
    lock.save(t).unwrap();
    new
}

/// The same spec from another provenance is another entry, never a member
/// squeezed into an entry that misstates where its plan came from — with or
/// without `--prune-sidecars`, and nothing a fit writes is deleted.
#[test]
fn the_same_plan_from_another_provenance_is_its_own_entry() {
    let (dir, t) = staged(true);
    fit(&t);
    let model = as_model_framed(&t, "2025-01.csv");

    // Without prune: March changes, is re-planned by the sniffer, and its
    // plan is recorded beside the model's rather than inside it.
    let march = dir.path().join("2025-03.csv");
    let mut body = std::fs::read(&march).unwrap();
    body.extend_from_slice(b"31.03.2025;Mitte;1'000.00\n");
    std::fs::write(&march, body).unwrap();
    let text = fit(&t);
    assert!(text.contains("REVIEW"), "the model's frame still asks:\n{text}");
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let m3 = lock.member("2025-03.csv", None, None).unwrap();
    assert!(m3.spec.is_some() && m3.spec.as_deref() != Some(model.as_str()), "{m3:?}");
    assert_eq!(lock.member("2025-01.csv", None, None).unwrap().spec.as_deref(), Some(model.as_str()));
    assert_eq!(sidecars(dir.path()), Vec::<String>::new());

    // With prune: a tool-written sidecar for February moves into the lock
    // as the sniffer's plan, and the file is gone because the lock holds it.
    ok(&tdy(&["fit", t.to_str().unwrap(), dir.path().join("2025-02.csv").to_str().unwrap()]));
    assert_eq!(sidecars(dir.path()), vec!["2025-02.csv.tdy.toml"]);
    let text = ok(&tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars"]));
    assert!(text.contains("--prune-sidecars: 1 moved into the lock and removed"), "{text}");
    assert_eq!(sidecars(dir.path()), Vec::<String>::new());
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let feb = lock.member("2025-02.csv", None, None).unwrap();
    assert_eq!(feb.spec, m3_spec(&lock), "February shares March's plan, not the model's");
    let out = tdy(&["check", t.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("has no spec") && err.contains("waiting on a human"), "{err}");
}

fn m3_spec(lock: &tdy::lockfile::Lock) -> Option<String> {
    lock.member("2025-03.csv", None, None).unwrap().spec.clone()
}

/// Provenance is part of what is recorded: a model-framed plan edited to
/// read as the sniffer's would drop the review its frame needs. Refused as
/// an edit, like any other.
#[test]
fn a_provenance_edited_in_the_lock_is_refused() {
    let (_dir, t) = staged(true);
    fit(&t);
    as_model_framed(&t, "2025-01.csv");
    let text = lock_text(&t);
    let edited = text.replacen("method = \"llm\"\n", "method = \"heuristic\"\n", 1).replacen("model = \"x\"\n", "", 1);
    assert_ne!(edited, text, "{text}");
    std::fs::write(tdy::lockfile::lock_path(&t), edited).unwrap();
    let out = query(&t, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("edited by hand"), "{err}");
}

/// Switching where plans are kept moves an accepted plan without changing
/// it, so the acceptance stays — lock to sidecars, and sidecars to lock.
#[test]
fn switching_storage_keeps_acceptances() {
    let (dir, t) = cents_pile();
    fit(&t);
    ok(&tdy(&["fit", t.to_str().unwrap(), "--accept", "2025-03.csv"]));
    let sql = std::fs::read_to_string(&t).unwrap();
    std::fs::write(&t, sql.replace("plans = 'lock'", "plans = 'sidecars'")).unwrap();
    let text = fit(&t);
    assert!(!text.contains("REVIEW"), "lock -> sidecars expired the acceptance:\n{text}");
    assert_eq!(sidecars(dir.path()).len(), 3);
    assert_eq!(lock_text(&t).matches("accepted = true").count(), 1);
    ok(&query(&t, "SELECT count(*) FROM dataset('@')"));

    std::fs::write(&t, sql).unwrap();
    let text = ok(&tdy(&["fit", t.to_str().unwrap(), "--prune-sidecars"]));
    assert!(!text.contains("REVIEW"), "sidecars -> lock expired the acceptance:\n{text}");
    assert!(text.contains("3 moved into the lock and removed"), "{text}");
    assert_eq!(lock_text(&t).matches("accepted = true").count(), 1);
    ok(&query(&t, "SELECT count(*) FROM dataset('@')"));
}

/// A sidecar that appears beside a lock-held member is read in place of the
/// plan the lock names — so until a fit records it, the two disagree, and
/// that is drift: the query stops and names it. The fit records the member
/// as sidecar-held, and the query serves what the sidecar reads.
#[test]
fn a_sidecar_beside_a_lock_held_member_is_drift_until_a_fit_records_it() {
    let (dir, t) = staged(true);
    fit(&t);
    let april = dir.path().join("2025-04.csv");
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let mut spec = lock.spec(lock.member("2025-04.csv", None, None).unwrap().spec.as_deref().unwrap()).unwrap().spec.clone();
    spec.transforms.insert(0, tdy::spec::Transform::SkipRows { head: 0, tail: 1 });
    tdy::sidecar::save(
        &april,
        &spec,
        tdy::sidecar::ProvenanceInfo {
            method: tdy::spec::InferenceMethod::Manual,
            model: None,
            prompt_version: None,
            sampled_bytes: None,
        },
    )
    .unwrap();
    let out = query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(err.contains("a sidecar now overrides the lock's plan for 2025-04.csv"), "{err}");

    fit(&t);
    let q = ok(&query(&t, "SELECT count(*) AS n, sum(amount) AS total FROM dataset('@')"));
    assert!(q.contains(" 35 ") && q.contains("55910.00"), "{q}");
    let lock = tdy::lockfile::Lock::load(&t).unwrap().unwrap();
    let m = lock.member("2025-04.csv", None, None).unwrap();
    assert!(m.spec.is_none() && !m.spec_digest.is_empty(), "{m:?}");
}

/// `tdy fit T FILE` under `plans = 'lock'` still writes the file's sidecar,
/// and says what that does to the pile.
#[test]
fn fitting_one_file_of_a_lock_pile_says_its_sidecar_overrides_the_lock() {
    let (dir, t) = staged(true);
    fit(&t);
    let jan = dir.path().join("2025-01.csv");
    let text = ok(&tdy(&["fit", t.to_str().unwrap(), jan.to_str().unwrap()]));
    assert!(text.contains("overrides the lock's plan for this member until `tdy fit"), "{text}");
    assert_eq!(sidecars(dir.path()), vec!["2025-01.csv.tdy.toml"]);
}

/// `check --against` and `validate` say about an edited lock plan what
/// `dataset()` says: refused, edited by hand — never CONFORMS or ok.
#[test]
fn check_and_validate_refuse_an_edited_lock_plan() {
    let (dir, t) = staged(true);
    fit(&t);
    let text = lock_text(&t);
    let edited = text.replacen("\"keine\",\n]\nthousands_separator", "\"keine\",\n    \"1'100.00\",\n]\nthousands_separator", 1);
    assert_ne!(edited, text);
    std::fs::write(tdy::lockfile::lock_path(&t), edited).unwrap();
    let jan = dir.path().join("2025-01.csv");
    let out = tdy(&["check", t.to_str().unwrap(), "--against", jan.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success() && !text.contains("CONFORMS"), "{text}");
    assert!(text.contains("EDITED") && text.contains("edited by hand"), "{text}");
    let out = tdy(&["validate", jan.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("edited by hand"), "{err}");
}
