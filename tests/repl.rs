//! `tdy` with piped stdin: the batch runner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn pile() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("2025-") && !n.ends_with(".tdy.toml") {
            std::fs::copy(e.path(), d.path().join(&n)).unwrap();
        }
    }
    d
}

fn run_script(dir: &Path, script: &str, args: &[&str]) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(args)
        .current_dir(dir)
        .env("TDY_BACKEND", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

#[test]
fn piped_stdin_runs_lines_and_prints_text() {
    let d = pile();
    let (code, out, _) = run_script(
        d.path(),
        ".sniff 2025-01.csv --no-llm\nSELECT count(*) AS n\nFROM messy('2025-01.csv');\n.ls\n",
        &[],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("preview (heuristic method"));
    assert!(out.contains("| n |") && out.contains("| 4 |"));
    assert!(out.contains("2025-01.csv") && out.contains("sniffed"));
}

#[test]
fn batch_stops_at_the_first_error_with_exit_one() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".nope\n.ls\n", &[]);
    assert_eq!(code, 1);
    assert!(out.contains("Error: unknown command `.nope`"));
    assert!(!out.contains("2025-01.csv"), "did not stop: {out}");
}

#[test]
fn tdy_console_subcommand_is_the_same_runner() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".help\n", &["console"]);
    assert_eq!(code, 0);
    assert!(out.contains(".sniff FILE"));
}

#[test]
fn quit_ends_the_script_cleanly() {
    let d = pile();
    let (code, out, _) = run_script(d.path(), ".quit\n.ls\n", &[]);
    assert_eq!(code, 0);
    assert!(!out.contains("2025-01.csv"));
}
