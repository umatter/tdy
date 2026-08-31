//! `tdy draft` — the scaffold, and the property that makes it more than
//! pretty printing: what it emits is valid target SQL, and over a pile that
//! shares a vocabulary the *unedited* draft already fits every file it was
//! drawn from. The judgements it cannot make (synonyms, absences) are laid
//! out as one-line edits, which the mixed-pile test pins.

use std::path::{Path, PathBuf};
use std::process::Command;

use tdy::config::Limits;
use tdy::target::Target;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn draft_of(names: &[&str]) -> String {
    let files: Vec<PathBuf> = names.iter().map(|n| corpus().join(n)).collect();
    tdy::draft::draft_target(&files, Limits::default()).expect("draftable")
}

/// The round trip: draft a pile with one shared vocabulary, change nothing,
/// and every file it was drawn from fits the draft. The draft is allowed to
/// be wrong about intent; it is not allowed to be wrong about what it saw.
#[test]
fn the_unedited_draft_fits_the_files_it_was_drawn_from() {
    let names = ["2025-01.csv", "2025-02.csv", "2025-03.csv", "2025-05.csv", "2025-06.csv"];
    let sql = draft_of(&names);
    let target = Target::parse(&sql).unwrap_or_else(|e| panic!("draft must parse:\n{sql}\n{e:#}"));
    for n in names {
        let p = corpus().join(n);
        if let Err(e) = tdy::fit::fit(&p, &target, Limits::default()) {
            panic!("{n} should fit the draft drawn from it:\n{e}\n--- draft ---\n{sql}");
        }
    }
}

/// The mixed pile: German and English files disagree on names, and the draft
/// must not pretend otherwise — both spellings appear as separate columns
/// with presence counts, so merging them is a visible one-line edit, never a
/// silent guess.
#[test]
fn synonyms_are_left_visible_not_guessed() {
    let sql = draft_of(&["2025-01.csv", "2025-02.csv", "2025-10.xlsx"]);
    Target::parse(&sql).unwrap_or_else(|e| panic!("draft must parse:\n{sql}\n{e:#}"));
    assert!(sql.contains("datum"), "{sql}");
    assert!(sql.contains("\n  date "), "{sql}");
    assert!(sql.contains("of 3 file(s)"), "presence must be stated:\n{sql}");
    assert!(sql.contains("date_order = 'dmy'"), "{sql}");
    // The verbatim spellings travel as matches.
    assert!(sql.contains("matches = 'Datum'"), "{sql}");
}

/// Types are merged by widening, and a widening is said out loud. Integers
/// in one file and decimals in another become DECIMAL (never DOUBLE — the
/// sniffer calls `1.5` a decimal precisely so money stays exact), and text
/// against anything is TEXT with the conflict named.
#[test]
fn disagreeing_types_widen_with_a_caveat() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.csv");
    let b = dir.path().join("b.csv");
    std::fs::write(&a, "menge\n1\n2\n3\n").unwrap();
    std::fs::write(&b, "menge\n1.5\n2.5\n3.5\n").unwrap();
    let sql = tdy::draft::draft_target(&[a.clone(), b], Limits::default()).unwrap();
    Target::parse(&sql).unwrap_or_else(|e| panic!("draft must parse:\n{sql}\n{e:#}"));
    assert!(sql.contains("DECIMAL"), "{sql}");
    assert!(sql.contains("widened"), "the widening must be said:\n{sql}");

    let c = dir.path().join("c.csv");
    std::fs::write(&c, "menge\nviel\nwenig\netwas\n").unwrap();
    let sql = tdy::draft::draft_target(&[a, c], Limits::default()).unwrap();
    assert!(sql.contains("TEXT"), "{sql}");
    assert!(sql.contains("kept TEXT"), "the conflict must be named:\n{sql}");
}

/// The CLI: prints the scaffold, and a pile with nothing sniffable is a real
/// error, not an empty CREATE TABLE.
#[test]
fn the_cli_prints_a_scaffold_and_refuses_an_unreadable_pile() {
    let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["draft", corpus().join("2025-01.csv").to_str().unwrap()])
        .output()
        .expect("run tdy");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("CREATE TABLE"), "{text}");
    assert!(text.contains("A DRAFT, not an answer"), "{text}");

    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.json");
    std::fs::write(&junk, "{\"not\": \"records\"}").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["draft", junk.to_str().unwrap()])
        .output()
        .expect("run tdy");
    assert!(!out.status.success(), "an undraftable pile must fail loudly");
}
