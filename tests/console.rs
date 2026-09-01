//! The console: grammar, session, and the promise that its text is the CLI's.

use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

/// A scratch copy of the drifting-exports pile: data files only, no
/// sidecars, no locks, no targets (each test writes the target it needs).
fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("2025-") && !n.ends_with(".tdy.toml") {
            std::fs::copy(e.path(), d.path().join(&n)).unwrap();
        }
    }
    std::fs::copy(corpus().join("sales.tdy.sql"), d.path().join("sales.tdy.sql")).unwrap();
    d
}

fn tdy(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .current_dir(dir)
        .env("TDY_BACKEND", "none")
        .output()
        .expect("run tdy")
}

fn no_llm() -> tdy::config::Config {
    tdy::config::load(&tdy::config::Overrides { backend: Some("none".into()), model: None, base_url: None })
        .unwrap()
}

#[tokio::test]
async fn sniff_text_is_what_the_binary_prints() {
    let d = pile();
    // An absolute path on both sides: `check_text`/`sniff_text` echo their
    // path argument verbatim (no canonicalization anywhere in the code), so
    // a relative CLI arg and an absolute in-process one would legitimately
    // print different paths without either side being wrong. Same file,
    // same argument form, is the actual thing under test.
    let file = d.path().join("2025-01.csv");
    let cli = tdy(d.path(), &[
        "sniff",
        file.to_str().unwrap(),
        "--no-llm",
    ]);
    assert!(cli.status.success());
    std::fs::remove_file(d.path().join("2025-01.csv.tdy.toml")).unwrap();

    let out = tdy::commands::sniff_text(
        &file,
        &no_llm(),
        tdy::provider::SniffCli { hint: None, force: false, no_llm: true, quick: false, json: false },
    )
    .await
    .unwrap();
    // The sidecar text embeds created_at, so compare with timestamps masked.
    let mask = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("created_at"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(mask(&out.text), mask(&String::from_utf8_lossy(&cli.stdout)));
    assert!(!out.kept_existing);
    assert_eq!(out.spec.columns.len(), 3);
}

#[test]
fn check_text_matches_binary_including_failure() {
    let d = pile();
    // Same reasoning as above: an absolute path on both sides so the two
    // calls describe literally the same argument.
    let target = d.path().join("sales.tdy.sql");
    // No lock yet: the "nothing to check" wording.
    let cli = tdy(d.path(), &["check", target.to_str().unwrap()]);
    let out = tdy::commands::check_text(&target, &[], no_llm().limits).unwrap();
    assert_eq!(out.text, String::from_utf8_lossy(&cli.stdout));
    assert!(out.ok);
}
