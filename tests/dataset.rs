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

// ---------------------------------------------------------------------------
// The review gate
// ---------------------------------------------------------------------------
//
// The sharpest line in the design: a plan whose acceptance rests on a
// *semantic* judgement rather than a mechanical proof does not run until a
// human makes that judgement. `decimal_shift = -2` is exact, lossless and
// self-evidencing, and it is still somebody's claim that this file's numbers
// mean something other than what they say. No proof settles that.

/// July's amounts are integer Rappen. A hand-written spec says so with
/// `decimal_shift = -2`. It conforms, it parses, every value is exact — and it
/// still does not join until somebody accepts it, because "these integers are
/// really hundredths" is a claim about the world, not about the bytes.
#[test]
fn a_value_changing_step_needs_a_human_and_then_works() {
    let dir = staged();
    // A target the Rappen file could otherwise never satisfy: nothing declares
    // `Betrag Rp.`, so the planner refuses it (see tests/fit.rs).
    let t = dir.path().join("rappen.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE rappen (\n\
         \x20 month      DATE          NOT NULL OPTIONS(matches = 'Datum'),\n\
         \x20 region     TEXT          NOT NULL OPTIONS(matches = 'Region'),\n\
         \x20 amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n\
         )\nWITH (files = '2025-07.csv', date_order = 'dmy');",
    )
    .unwrap();

    let july = dir.path().join("2025-07.csv");
    write_rappen_spec(&july);
    assert!(tdy(&["validate", july.to_str().unwrap(), "--stamp"]).status.success());

    // Fitting records the review and writes the lock, but does not accept.
    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("REVIEW"), "{text}");
    assert!(text.contains("decimal_shift"), "{text}");
    assert!(text.contains("--accept"), "no remedy offered:\n{text}");

    // …and the query refuses until somebody does.
    let q = format!("SELECT sum(amount_chf) FROM dataset('{}')", t.display());
    let blocked = tdy(&["query", &q]);
    let err = String::from_utf8_lossy(&blocked.stderr);
    assert!(!blocked.status.success(), "an unaccepted value change was queried");
    assert!(err.contains("waiting on a human"), "{err}");
    assert!(err.contains("2025-07.csv"), "{err}");

    // Accept, and it joins — at the right magnitude, not a hundred times out.
    let acc = tdy(&["fit", t.to_str().unwrap(), "--accept", "2025-07.csv"]);
    assert!(acc.status.success(), "{}", String::from_utf8_lossy(&acc.stderr));
    let ok = tdy(&["query", &q]);
    let text = String::from_utf8_lossy(&ok.stdout);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    // July: 1700 + 1710 + 1720 + 1730 = 6860.00, in francs.
    assert!(text.contains("6860.00"), "wrong magnitude — check the shift:\n{text}");
}

/// Asking the same question every run would train people to answer it without
/// reading, so an acceptance carries over while nothing has changed.
#[test]
fn an_acceptance_carries_over_but_expires_when_the_file_changes() {
    let dir = staged();
    let t = dir.path().join("rappen.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE rappen (\n\
         \x20 month      DATE          NOT NULL OPTIONS(matches = 'Datum'),\n\
         \x20 region     TEXT          NOT NULL OPTIONS(matches = 'Region'),\n\
         \x20 amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n\
         )\nWITH (files = '2025-07.csv', date_order = 'dmy');",
    )
    .unwrap();
    let july = dir.path().join("2025-07.csv");
    write_rappen_spec(&july);
    assert!(tdy(&["validate", july.to_str().unwrap(), "--stamp"]).status.success());
    assert!(tdy(&["fit", t.to_str().unwrap()]).status.success());
    assert!(tdy(&["fit", t.to_str().unwrap(), "--accept", "2025-07.csv"]).status.success());

    // Re-fitting an untouched dataset must not ask again.
    let again = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&again.stdout);
    assert!(text.contains("accepted"), "the acceptance was not carried over:\n{text}");
    assert!(!text.contains("REVIEW"), "{text}");

    // Change the file: the acceptance was about *those* bytes.
    let mut body = std::fs::read(&july).unwrap();
    body.extend_from_slice("31.07.2025;Ost;999900\n".as_bytes());
    std::fs::write(&july, body).unwrap();
    assert!(tdy(&["validate", july.to_str().unwrap(), "--stamp"]).status.success());

    let after = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&after.stdout);
    assert!(text.contains("REVIEW"), "the acceptance survived an edit:\n{text}");
}

/// A hand-written spec is a human assertion about specific bytes; the planner
/// must not overwrite it — but it is proved exactly as a planned one is.
#[test]
fn a_manual_spec_is_kept_but_still_proved() {
    let dir = staged();
    let t = target_of(&dir);
    fit_all(&dir);

    // Replace a fitted sidecar with a hand-written one that does NOT conform.
    let p = dir.path().join("2025-01.csv");
    let sc = tdy::sidecar::sidecar_path(&p);
    let text = std::fs::read_to_string(&sc).unwrap();
    let manual = text
        .replace("method = \"heuristic\"", "method = \"manual\"")
        .replace("name = \"region\"", "name = \"gebiet\"");
    std::fs::write(&sc, manual).unwrap();
    assert!(tdy(&["validate", p.to_str().unwrap(), "--stamp"]).status.success());

    let out = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "a non-conforming manual spec was accepted:\n{text}");
    assert!(text.contains("CONTRADICTS"), "{text}");
    assert!(text.contains("hand-written"), "{text}");
}

/// A hand-written spec for `2025-07.csv`: reads `Betrag Rp.` and shifts the
/// decimal point two places left, turning integer Rappen into francs.
fn write_rappen_spec(csv: &Path) {
    let sc = tdy::sidecar::sidecar_path(csv);
    std::fs::write(
        &sc,
        r#"spec_version = 1
[source]
path = "2025-07.csv"
blake3 = "0"
bytes = 0
[provenance]
method = "manual"
tool_version = "0.1.0"
created_at = "2026-01-01T00:00:00Z"
[spec]
[spec.extraction]
format = "delimited"
delimiter = ";"
quote = '"'
encoding = "windows-1252"
ragged = "pad_nulls"
[[spec.transforms]]
op = "promote_header"
rows = 1
join = " "
[[spec.columns]]
name = "month"
source = "Datum"
nullable = false
[spec.columns.dtype]
type = "date"
format = "%d.%m.%Y"
[[spec.columns]]
name = "region"
source = "Region"
nullable = false
[spec.columns.dtype]
type = "utf8"
[[spec.columns]]
name = "amount_chf"
source = "Betrag Rp."
nullable = false
[spec.columns.parse]
decimal_shift = -2
[spec.columns.dtype]
type = "decimal"
precision = 14
scale = 2
"#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// The lock as an operational contract. Each of these was a way for a gate to
// pass while the thing it gates was wrong.
// ---------------------------------------------------------------------------

/// `--accept` names a member — the path the lock records, relative to the
/// target — not a basename.
///
/// Matching on the basename meant `--accept jan.csv` accepted whichever
/// `jan.csv` came first when two directories held one, and a member inside a
/// subdirectory could never be accepted at all: its lock entry is `sub/jan.csv`
/// and no basename ever equals that.
#[test]
fn accept_names_a_member_and_refuses_a_stranger() {
    let dir = staged();
    let t = target_of(&dir);
    // A file that is not a member at all.
    let out = tdy(&["fit", t.to_str().unwrap(), "--accept", "2025-07.csv"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "accepting a non-member must fail");
    assert!(err.contains("not a member"), "{err}");
}

/// A member in a subdirectory can be accepted, spelt the way the lock spells
/// it. This is the case the basename match could not express at all.
#[test]
fn a_member_in_a_subdirectory_can_be_accepted() {
    let dir = staged();
    let sub = dir.path().join("exports");
    std::fs::create_dir(&sub).unwrap();
    let july = sub.join("2025-07.csv");
    std::fs::copy(dir.path().join("2025-07.csv"), &july).unwrap();
    std::fs::remove_file(dir.path().join("2025-07.csv")).unwrap();
    write_rappen_spec(&july);
    assert!(tdy(&["validate", july.to_str().unwrap(), "--stamp"]).status.success());

    let target = dir.path().join("rappen.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE rappen (
           month      DATE          NOT NULL OPTIONS(matches = 'Datum'),
           region     TEXT          NOT NULL OPTIONS(matches = 'Region'),
           amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
         ) WITH (files = 'exports/*.csv', date_order = 'dmy');",
    )
    .unwrap();

    let ts = target.to_str().unwrap();
    let out = tdy(&["fit", ts]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("REVIEW"), "the shift should need a human:\n{text}");

    // The lock's own spelling, which is what --accept must take.
    let out = tdy(&["fit", ts, "--accept", "exports/2025-07.csv"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let q = tdy(&["query", &format!("SELECT count(*) FROM dataset('{ts}')")]);
    assert!(q.status.success(), "{}", String::from_utf8_lossy(&q.stderr));
}

/// An acceptance is a judgement about a *plan*. Editing the plan afterwards
/// must retract it — the data has not changed, so nothing else would notice.
#[test]
fn editing_an_accepted_members_spec_retracts_the_acceptance() {
    let dir = staged();
    // 2025-07 is in Rappen and is excluded from sales_ok; give it its own
    // target so it becomes an accepted member.
    let july = dir.path().join("2025-07.csv");
    write_rappen_spec(&july);
    assert!(tdy(&["validate", july.to_str().unwrap(), "--stamp"]).status.success());

    let target = dir.path().join("rappen.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE rappen (
           month      DATE          NOT NULL OPTIONS(matches = 'Datum'),
           region     TEXT          NOT NULL OPTIONS(matches = 'Region'),
           amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
         ) WITH (files = '2025-07.csv', date_order = 'dmy');",
    )
    .unwrap();
    let ts = target.to_str().unwrap();
    let acc = tdy(&["fit", ts, "--accept", "2025-07.csv"]);
    assert!(
        acc.status.success(),
        "{}{}",
        String::from_utf8_lossy(&acc.stdout),
        String::from_utf8_lossy(&acc.stderr)
    );
    let q = tdy(&["query", &format!("SELECT count(*) FROM dataset('{ts}')")]);
    assert!(q.status.success(), "{}", String::from_utf8_lossy(&q.stderr));

    // Now change the very thing that was accepted: the shift.
    let sc = dir.path().join("2025-07.csv.tdy.toml");
    let text = std::fs::read_to_string(&sc).unwrap();
    let tampered = text.replace("decimal_shift = -2", "decimal_shift = -3");
    assert_ne!(tampered, text, "the fixture changed shape; update this test");
    std::fs::write(&sc, tampered).unwrap();
    let p = dir.path().join("2025-07.csv");
    assert!(tdy(&["validate", p.to_str().unwrap(), "--stamp"]).status.success());

    let q = tdy(&["query", &format!("SELECT count(*) FROM dataset('{ts}')")]);
    let err = String::from_utf8_lossy(&q.stderr);
    assert!(!q.status.success(), "an edited acceptance was still honoured");
    assert!(err.contains("2025-07.csv"), "{err}");
    assert!(err.contains("accepted"), "{err}");
}

/// `tdy check <TARGET>` with no `--against` is the documented CI gate. Once a
/// lock exists it must actually check it: it used to print "nothing to check"
/// and exit zero, so the gate passed on a dataset that could not be queried.
#[test]
fn check_with_a_lock_is_a_real_gate() {
    let dir = staged();
    fit_all(&dir);
    let t = target_of(&dir);
    let ts = t.to_str().unwrap();

    let ok = tdy(&["check", ts]);
    assert!(
        ok.status.success(),
        "a freshly fitted dataset must pass:\n{}{}",
        String::from_utf8_lossy(&ok.stdout),
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(String::from_utf8_lossy(&ok.stdout).contains("conforming"));

    // Change a member. The gate must now fail, naming it.
    let f = dir.path().join("2025-01.csv");
    let mut text = std::fs::read_to_string(&f).unwrap();
    text.push_str("15.01.2025;Bern;99.00\n");
    std::fs::write(&f, text).unwrap();

    let bad = tdy(&["check", ts]);
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(!bad.status.success(), "a gate that exits zero on drift is not a gate");
    assert!(err.contains("2025-01.csv"), "{err}");
}

/// A target named by its bare filename resolves the same members as one named
/// by a path. `Path::parent` of `sales.tdy.sql` is `Some("")`, not `None`, and
/// `read_dir("")` fails — so every drift check passed vacuously.
#[test]
fn a_bare_target_filename_resolves_its_members() {
    let dir = staged();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["fit", "sales_ok.tdy.sql"])
        .current_dir(dir.path())
        .output()
        .expect("run tdy");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("9 file(s) match"), "no members were found:\n{text}");
}

/// A path listed twice would be read twice: a dataset whose total is silently
/// doubled for one member.
#[test]
fn a_member_listed_twice_is_drift() {
    let dir = staged();
    fit_all(&dir);
    let lock = dir.path().join("sales_ok.tdy.lock");
    let text = std::fs::read_to_string(&lock).unwrap();
    // Duplicate the first member block by hand — a merge conflict resolved
    // badly looks exactly like this.
    let first = text
        .split("[[member]]")
        .nth(1)
        .expect("the lock must have members")
        .to_string();
    let first = first.split("\n[[").next().unwrap().to_string();
    std::fs::write(&lock, format!("{text}\n[[member]]{first}")).unwrap();

    let out = query(&dir, "SELECT count(*) FROM dataset('@')");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a doubled member was read twice");
    assert!(err.contains("twice"), "{err}");
}

/// An `exclude` naming no directory applies in every directory. Requiring it
/// to repeat the `files=` prefix made it silently do nothing, which is the
/// worst possible failure for a subtraction.
#[test]
fn an_exclude_without_a_directory_applies_everywhere() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("exports");
    std::fs::create_dir(&sub).unwrap();
    for name in ["jan.csv", "jan-draft.csv"] {
        std::fs::write(sub.join(name), "Datum;Region\n05.07.2025;Bern\n").unwrap();
    }
    let target = dir.path().join("t.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE t (
           month  DATE NOT NULL OPTIONS(matches = 'Datum'),
           region TEXT NOT NULL OPTIONS(matches = 'Region')
         ) WITH (files = 'exports/*.csv', exclude = '*-draft.csv', date_order = 'dmy');",
    )
    .unwrap();
    let out = tdy(&["fit", target.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1 file(s) match"), "the draft was not excluded:\n{text}");
}

/// A hand-written constant *value* — "November is all Ticino" — is data the
/// file does not contain, asserted by a human. It conforms, it parses, and it
/// still does not join until somebody accepts it, exactly like the Rappen
/// shift: tdy cannot check a fact about the world.
#[test]
fn a_constant_value_needs_a_human_and_then_works() {
    let dir = staged();
    let t = dir.path().join("nov.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE nov (\n\
         \x20 month      DATE          NOT NULL OPTIONS(matches = 'Datum'),\n\
         \x20 region     TEXT          NULL,\n\
         \x20 amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')\n\
         )\nWITH (files = '2025-11.csv', date_order = 'dmy');",
    )
    .unwrap();

    let nov = dir.path().join("2025-11.csv");
    let sc = tdy::sidecar::sidecar_path(&nov);
    std::fs::write(
        &sc,
        r#"spec_version = 1
[source]
path = "2025-11.csv"
blake3 = "0"
bytes = 0
[provenance]
method = "manual"
tool_version = "0.1.0"
created_at = "2026-01-01T00:00:00Z"
[spec]
[spec.extraction]
format = "delimited"
delimiter = ";"
quote = '"'
ragged = "pad_nulls"
[[spec.transforms]]
op = "promote_header"
rows = 1
join = " "
[[spec.transforms]]
op = "constant"
name = "region"
value = "Ticino"
[[spec.columns]]
name = "month"
source = "Datum"
nullable = false
[spec.columns.dtype]
type = "date"
format = "%d.%m.%Y"
[[spec.columns]]
name = "region"
nullable = true
[spec.columns.dtype]
type = "utf8"
[[spec.columns]]
name = "amount_chf"
source = "Betrag"
nullable = false
[spec.columns.dtype]
type = "decimal"
precision = 14
scale = 2
[spec.columns.parse]
thousands_separator = "'"
decimal_separator = "."
"#,
    )
    .unwrap();
    assert!(tdy(&["validate", nov.to_str().unwrap(), "--stamp"]).status.success());

    let ts = t.to_str().unwrap();
    let out = tdy(&["fit", ts]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("REVIEW"), "{text}");
    assert!(text.contains("Ticino"), "the asserted value must be shown:\n{text}");

    let q = format!(
        "SELECT region, count(*) FROM dataset('{}') GROUP BY 1",
        t.display()
    );
    let blocked = tdy(&["query", &q]);
    assert!(!blocked.status.success(), "an unaccepted constant was queried");

    let acc = tdy(&["fit", ts, "--accept", "2025-11.csv"]);
    assert!(acc.status.success(), "{}", String::from_utf8_lossy(&acc.stderr));
    let ok = tdy(&["query", &q]);
    let text = String::from_utf8_lossy(&ok.stdout);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    assert!(text.contains("Ticino"), "{text}");
}
