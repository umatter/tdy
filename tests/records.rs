//! A document is a record: a directory of one-object JSON documents is one
//! table. The pile is `testdata/json_records/` (generator
//! `20_json_records.py`, whose docstring holds the ground truth), modelled
//! on villagerdb's items: some describe the item in `games.nh`, some in
//! `games.nl`, one holds arrays the planner must eliminate.
//!
//! The assertions are sums, not shapes: a planner that read 1-up-cap.json as
//! its one-row table of buy prices would still conform and still execute,
//! and would be caught here.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

fn pile() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("json_records")
}

/// A private copy, so sidecars and the lock land in scratch.
fn staged() -> TempDir {
    let dir = TempDir::new().unwrap();
    for e in std::fs::read_dir(pile()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if e.path().is_file() && !n.ends_with(".tdy.toml") && !n.ends_with(".tdy.lock") {
            std::fs::copy(e.path(), dir.path().join(&n)).unwrap();
        }
    }
    dir
}

fn tdy(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn ok(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    text
}

/// The vision for the 7,900: seven documents, one relation, the generator's
/// sums. Nothing waits on a person — the record frame is a proof, by
/// elimination where the document also holds arrays.
#[test]
fn a_directory_of_documents_is_one_table_with_the_right_sums() {
    let dir = staged();
    let t = dir.path().join("items.tdy.sql");
    let fit = ok(&tdy(&["fit", t.to_str().unwrap()]));
    assert!(fit.contains("7 member(s)") || fit.contains("7 file(s)"), "{fit}");
    assert!(!fit.contains("REVIEW"), "{fit}");
    // The binding says where inside the key the value was read.
    assert!(fit.contains(r#"nh_sell<-"games"/nh/sellPrice/value"#), "{fit}");
    let json = ok(&tdy(&["--json", "fit", t.to_str().unwrap(), "--dry-run"]));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let acorn = v["members"].as_array().unwrap().iter().find(|m| m["path"] == "acorn.json").unwrap();
    let nh = acorn["sources"].as_array().unwrap().iter().find(|s| s["column"] == "nh_sell").unwrap();
    assert_eq!((nh["source"].as_str(), nh["pointer"].as_str()), (Some("games"), Some("/nh/sellPrice/value")), "{nh}");
    let id = acorn["sources"].as_array().unwrap().iter().find(|s| s["column"] == "id").unwrap();
    assert!(id.get("pointer").is_none(), "{id}");

    let sql = format!(
        "SELECT count(*) n, sum(nh_sell) nh, sum(nl_sell) nl, count(nh_sell) nh_n, count(nl_sell) nl_n \
         FROM dataset('{}')",
        t.display()
    );
    let text = ok(&tdy(&["query", &sql]));
    let row = text.lines().find(|l| l.contains("1780")).unwrap_or_else(|| panic!("{text}"));
    let cells: Vec<&str> = row.split('|').map(str::trim).filter(|c| !c.is_empty()).collect();
    assert_eq!(cells, ["7", "1780", "255", "4", "3"], "{text}");

    // The document with arrays is read as the item, not as its buy prices.
    let sql = format!("SELECT name, category, nl_sell FROM dataset('{}') WHERE id = '1-up-cap'", t.display());
    let text = ok(&tdy(&["query", &sql]));
    assert!(text.contains("1-up Cap") && text.contains("Hats") && text.contains(" 80 "), "{text}");
}

/// One sidecar per member, and the one with arrays says how its frame was
/// chosen.
#[test]
fn the_member_with_arrays_is_a_record_by_elimination() {
    let dir = staged();
    let t = tdy::target::Target::load(&dir.path().join("items.tdy.sql")).unwrap();
    let cap = tdy::fit::fit(&dir.path().join("1-up-cap.json"), &t, tdy::config::Limits::default()).unwrap();
    assert!(
        matches!(cap.spec.extraction, tdy::spec::Extraction::Json { record: true, pointer: None, .. }),
        "{:?}",
        cap.spec.extraction
    );
    let proof = cap.notes.iter().find(|n| n.contains("elimination")).unwrap_or_else(|| panic!("{:?}", cap.notes));
    assert!(proof.contains("the document as one record") && proof.contains("of 4 candidate"), "{proof}");
    assert!(cap.review.is_none());

    let acorn = tdy::fit::fit(&dir.path().join("acorn.json"), &t, tdy::config::Limits::default()).unwrap();
    assert!(!acorn.notes.iter().any(|n| n.contains("elimination")), "{:?}", acorn.notes);
}

/// The corpus flow in miniature: draft the pile, make exactly the edits its
/// comments call for (`if_missing = 'null'` on every leaf some documents
/// lack), fit, query.
#[test]
fn draft_then_the_edits_it_names_then_fit_then_query() {
    let dir = staged();
    std::fs::remove_file(dir.path().join("items.tdy.sql")).unwrap();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let draft = tdy::draft::draft_target_in(&files, Some(dir.path()), tdy::config::Limits::default()).unwrap();
    assert!(draft.contains("games/nl/buyPrices"), "the nested arrays are named:\n{draft}");

    // The edit the comments call for, and nothing else.
    let edited: String = draft
        .lines()
        .map(|l| {
            if !l.contains(" of 7 file(s)") {
                return format!("{l}\n");
            }
            let (decl, comment) = l.split_once("  --").unwrap();
            let comma = decl.ends_with(',');
            let decl = decl.trim_end_matches(',');
            let decl = match decl.strip_suffix(')') {
                Some(head) if decl.contains("OPTIONS(") => format!("{head}, if_missing = 'null')"),
                _ => format!("{decl} OPTIONS(if_missing = 'null')"),
            };
            format!("{decl}{}  --{comment}\n", if comma { "," } else { "" })
        })
        .collect();
    let t = dir.path().join("items.tdy.sql");
    std::fs::write(&t, &edited).unwrap();
    let fit = tdy(&["fit", t.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&fit.stdout);
    assert!(fit.status.success(), "{text}{}\n--- target ---\n{edited}", String::from_utf8_lossy(&fit.stderr));

    let sql = format!(
        "SELECT count(*) n, sum(games_nh_sellprice_value) nh, sum(games_nl_sellprice_value) nl FROM dataset('{}')",
        t.display()
    );
    let text = ok(&tdy(&["query", &sql]));
    let row = text.lines().find(|l| l.contains("1780")).unwrap_or_else(|| panic!("{text}\n{edited}"));
    let cells: Vec<&str> = row.split('|').map(str::trim).filter(|c| !c.is_empty()).collect();
    assert_eq!(cells, ["7", "1780", "255"], "{text}");
}
