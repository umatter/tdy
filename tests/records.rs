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

/// `--json`: an ambiguous frame's choices are the settings that choose,
/// each usable as written.
#[test]
fn ambiguous_frame_choices_are_settings() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("both.json"),
        r#"{"id":"top","name":"Report","rows":[{"id":"a","name":"Ann"},{"id":"b","name":"Bo"}]}"#,
    )
    .unwrap();
    let t = dir.path().join("t.tdy.sql");
    std::fs::write(&t, "CREATE TABLE t (id TEXT NOT NULL, name TEXT NOT NULL) WITH (files = '*.json');\n").unwrap();
    let out = tdy(&["--json", "fit", t.to_str().unwrap(), "--dry-run"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let p = &v["members"][0]["problems"][0];
    assert_eq!(p["kind"], "ambiguous_frame", "{v:#}");
    assert_eq!(p["choices"], serde_json::json!(["record = true", "pointer = \"/rows\""]), "{v:#}");
}

fn fw2() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("jan.json"), r#"{"id":100,"name":"export-jan","rows":[{"id":1,"name":"a"},{"id":2,"name":"b"}]}"#).unwrap();
    std::fs::write(dir.path().join("feb.json"), r#"{"id":101,"name":"export-feb","rows":[]}"#).unwrap();
    std::fs::write(
        dir.path().join("t.tdy.sql"),
        "CREATE TABLE t (id BIGINT NOT NULL, name TEXT NOT NULL) WITH (files = '*.json');\n",
    )
    .unwrap();
    dir
}

/// January is ambiguous until a person settles it with `pointer = "/rows"`.
/// February's `rows` is empty: zero records. The only frame that "fits" it is
/// the document as one record, because an empty array has no header — so
/// the elimination left out the zero-row reading, and the record reading is a
/// judgement: one row of envelope data (101, export-feb) beside January's
/// records unless a person says the document is the record.
#[test]
fn a_record_chosen_beside_an_empty_array_waits_on_a_person() {
    let dir = fw2();
    let t = dir.path().join("t.tdy.sql");
    let first = tdy(&["fit", t.to_str().unwrap()]);
    assert!(!first.status.success(), "jan is an ambiguous frame");
    // The person settles January, as the message says.
    let jan = tdy(&["sniff", dir.path().join("jan.json").to_str().unwrap(), "--no-llm"]);
    assert!(jan.status.success());
    let side = dir.path().join("jan.json.tdy.toml");
    let text = std::fs::read_to_string(&side).unwrap();
    // The sniffer already reads `/rows`; marking it manual is the person's
    // choice of that frame.
    assert!(text.contains("pointer = \"/rows\""), "{text}");
    let text = text.replace("method = \"heuristic\"", "method = \"manual\"").replace("nullable = true", "nullable = false");
    std::fs::write(&side, text).unwrap();

    let fit = ok(&tdy(&["fit", t.to_str().unwrap()]));
    assert!(fit.contains("REVIEW"), "{fit}");
    assert!(fit.contains("`/rows` is empty — zero records"), "{fit}");

    let sql = format!("SELECT id, name FROM dataset('{}') ORDER BY id", t.display());
    let refused = tdy(&["query", &sql]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("feb.json"), "{}", String::from_utf8_lossy(&refused.stderr));

    ok(&tdy(&["fit", t.to_str().unwrap(), "--accept", "feb.json"]));
    let rows = ok(&tdy(&["query", &sql]));
    for want in ["| 1 ", "| 2 ", "| 101 "] {
        assert!(rows.contains(want), "{want}:\n{rows}");
    }

    // A person who writes `record = true` by hand has made the judgement.
    let feb = dir.path().join("feb.json.tdy.toml");
    let text = std::fs::read_to_string(&feb).unwrap().replace("method = \"heuristic\"", "method = \"manual\"");
    assert!(text.contains("record = true"), "{text}");
    std::fs::write(&feb, text).unwrap();
    let fit = ok(&tdy(&["fit", t.to_str().unwrap()]));
    assert!(!fit.contains("REVIEW"), "{fit}");
}
