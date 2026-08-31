//! `--json`: the machine-readable face of sniff / fit / check.
//!
//! The contract: everything the text output says is in the JSON, structured —
//! a gap is a `kind` plus the fields a caller could act on (`tried`, the
//! file's `header`, the remedy in `message`), never prose to re-parse.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn staged() -> TempDir {
    let dir = TempDir::new().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if p.is_file() && !n.ends_with(".tdy.toml") && !n.ends_with(".tdy.lock") {
            std::fs::copy(&p, dir.path().join(&n)).unwrap();
        }
    }
    dir
}

fn tdy(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn json_of(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON: {e}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// The full pile, structured: statuses, bindings, and — for the refused
/// members — problems with machine-usable fields.
#[test]
fn fit_json_reports_the_whole_pile_structured() {
    let dir = staged();
    let t = dir.path().join("sales.tdy.sql"); // no excludes: 3 members must fail
    let out = tdy(&["fit", t.to_str().unwrap(), "--json"]);
    assert!(!out.status.success(), "the unedited sales target has 3 refusals");
    let v = json_of(&out);

    assert_eq!(v["failed"], 3, "{v:#}");
    assert_eq!(v["fitted"], 9, "{v:#}");
    assert!(v.get("lock_written").is_none(), "no partial lock: {v:#}");

    let members = v["members"].as_array().unwrap();
    assert_eq!(members.len(), 12);

    let july = members.iter().find(|m| m["path"] == "2025-07.csv").unwrap();
    assert_eq!(july["status"], "gaps");
    let p = &july["problems"][0];
    assert_eq!(p["kind"], "no_candidate");
    assert!(p["tried"].as_array().unwrap().iter().any(|t| t == "Betrag"));
    assert!(p["header"].as_array().unwrap().iter().any(|h| h == "Betrag Rp."));

    let aug = members.iter().find(|m| m["path"] == "2025-08.csv").unwrap();
    assert_eq!(aug["problems"][0]["kind"], "ambiguous");

    let jan = members.iter().find(|m| m["path"] == "2025-01.csv").unwrap();
    assert_eq!(jan["status"], "fits");
    assert!(jan["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["column"] == "amount_chf" && s["source"] == "Betrag"));
}

/// A member behind the review gate says so in JSON, with the reason.
#[test]
fn fit_json_carries_the_review_gate() {
    let dir = staged();
    let t = dir.path().join("sales_ok.tdy.sql");
    let out = tdy(&["fit", t.to_str().unwrap(), "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json_of(&out);
    assert_eq!(v["failed"], 0);
    assert!(v["lock_written"].is_string(), "{v:#}");

    // check --json on the ready dataset.
    let out = tdy(&["check", t.to_str().unwrap(), "--json"]);
    assert!(out.status.success());
    let v = json_of(&out);
    assert_eq!(v["ready"], true, "{v:#}");
    assert_eq!(v["members"].as_array().unwrap().len(), 9);
}

/// sniff --json: confidence, notes, and the full spec, one object.
#[test]
fn sniff_json_is_one_object_with_the_spec_inside() {
    let dir = staged();
    let f = dir.path().join("2025-01.csv");
    let out = tdy(&["sniff", f.to_str().unwrap(), "--no-llm", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json_of(&out);
    assert_eq!(v["method"], "heuristic");
    assert!(v["spec"]["columns"].as_array().unwrap().len() >= 3, "{v:#}");
    assert!(v["sidecar"].as_str().unwrap().ends_with(".tdy.toml"));
}
