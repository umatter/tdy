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

use tdy::console::{EntryKind, EntryStatus, Payload, Session};

async fn session(dir: &Path) -> Session {
    Session::new(dir, no_llm()).unwrap()
}

#[tokio::test]
async fn help_quit_and_unknown() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".help", None).await;
    assert!(o.ok);
    assert!(o.text.contains(".sniff FILE") && o.text.contains(".fit TARGET"));
    let o = s.run(".nope", None).await;
    assert!(!o.ok);
    assert_eq!(o.text, "Error: unknown command `.nope` — `.help` lists them\n");
    assert!(matches!(o.payload, Payload::Error { .. }));
    let o = s.run(".quit", None).await;
    assert!(matches!(o.payload, Payload::Quit) && s.wants_quit());
}

#[tokio::test]
#[ignore = "enabled in Task 6"]
async fn ls_hides_companions_and_reports_status() {
    let d = pile();
    std::fs::create_dir(d.path().join("archive")).unwrap();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    // Stale: sidecar written, then the file changes.
    s.run(".sniff 2025-02.csv --no-llm", None).await;
    std::fs::write(d.path().join("2025-02.csv"), "Datum;Region;Betrag\n01.02.2025;Ost;1\n").unwrap();

    let o = s.run(".ls", None).await;
    assert!(o.ok);
    let Payload::Listing(entries) = o.payload else { panic!("{:?}", o.payload) };
    let find = |n: &str| entries.iter().find(|e| e.name == n).unwrap_or_else(|| panic!("{n} missing"));
    assert_eq!(find("archive/").kind, EntryKind::Dir);
    assert!(matches!(find("2025-01.csv").status, EntryStatus::Sniffed { .. }));
    assert_eq!(find("2025-02.csv").status, EntryStatus::Stale);
    assert_eq!(find("2025-07.csv").status, EntryStatus::None);
    assert_eq!(find("sales.tdy.sql").kind, EntryKind::Target);
    assert_eq!(find("sales.tdy.sql").status, EntryStatus::NoLock);
    assert!(entries.iter().all(|e| !e.name.ends_with(".tdy.toml")));
    assert!(o.text.contains("2025-02.csv") && o.text.contains("stale"));
}

#[tokio::test]
async fn cd_stays_inside_the_root() {
    let d = pile();
    std::fs::create_dir(d.path().join("archive")).unwrap();
    let mut s = session(d.path()).await;
    assert!(s.run(".cd archive", None).await.ok);
    assert!(s.cwd().ends_with("archive"));
    assert!(s.run(".cd ..", None).await.ok);
    let o = s.run(".cd ..", None).await;
    assert!(!o.ok && o.text.contains("outside"));
    let o = s.run(".sniff ../../etc/passwd", None).await;
    assert!(!o.ok && o.text.contains("outside"));
}

#[tokio::test]
async fn a_missing_file_is_a_typo_not_an_escape() {
    let d = pile();
    let mut s = session(d.path()).await;
    // In the root, but never written: an ordinary typo.
    let o = s.run(".sniff typo.csv", None).await;
    assert!(!o.ok);
    assert!(o.text.contains("does not exist"), "{}", o.text);
    assert!(!o.text.contains("outside"), "{}", o.text);

    let o = s.run(".cd nope_dir", None).await;
    assert!(!o.ok);
    assert!(o.text.contains("does not exist"), "{}", o.text);
    assert!(!o.text.contains("outside"), "{}", o.text);
}
