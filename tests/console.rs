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

#[tokio::test]
async fn sniff_writes_the_sidecar_and_returns_a_summary() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".sniff 2025-01.csv --no-llm", None).await;
    assert!(o.ok, "{}", o.text);
    assert_eq!(o.echo, ".sniff 2025-01.csv --no-llm");
    assert!(d.path().join("2025-01.csv.tdy.toml").exists());
    let Payload::Sniffed { spec, preview, kept_existing, .. } = o.payload else { panic!() };
    assert!(!kept_existing);
    assert_eq!(spec.columns.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["datum", "region", "betrag"]);
    assert_eq!(spec.columns[2].2, "DECIMAL(38,2)");
    assert_eq!(preview.columns, ["datum", "region", "betrag"]);
    assert_eq!(preview.rows[0], ["2025-01-31", "Ost", "1100.00"]);
    assert!(o.text.contains("preview (heuristic method, confidence 0.95)"));

    // A second sniff keeps the fresh sidecar and says so.
    let o = s.run(".sniff 2025-01.csv --no-llm", None).await;
    assert!(o.ok);
    assert!(o.text.starts_with("note: ") && o.text.contains("--force to re-infer"));
    let Payload::Sniffed { kept_existing, .. } = o.payload else { panic!() };
    assert!(kept_existing);
}

#[tokio::test]
async fn validate_and_show() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".validate 2025-01.csv", None).await;
    assert!(!o.ok && o.text.contains("no sidecar"));
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run(".validate 2025-01.csv", None).await;
    assert!(o.ok && o.text.contains(": ok"));

    let o = s.run(".show 2025-07.csv", None).await; // no sidecar
    assert!(o.ok, "{}", o.text);
    let Payload::Shown { raw, spec, .. } = o.payload else { panic!() };
    assert_eq!(raw.lines[0], "Datum;Region;Betrag Rp.");
    assert!(spec.is_none());
    assert!(o.text.contains("Datum;Region;Betrag Rp.") && o.text.contains("no sidecar"));

    let o = s.run(".show 2025-01.csv", None).await; // with sidecar
    let Payload::Shown { spec: Some(sp), .. } = o.payload else { panic!() };
    assert_eq!(sp.columns[0].1, "Datum");

    let o = s.run(".show 2025-09.xlsx", None).await;
    let Payload::Shown { raw, .. } = o.payload else { panic!() };
    assert_eq!(raw.sheets.len(), 1);
    assert_eq!(raw.sheets[0].0, "Umsatz");
}

#[tokio::test]
async fn draft_prints_or_writes_and_never_overwrites() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".draft 2025-*.csv 2025-*.xlsx", None).await;
    assert!(o.ok, "{}", o.text);
    // The line as actually run: globs expanded, root-relative, in match
    // order (all `2025-*.csv` hits, then all `2025-*.xlsx` hits).
    assert_eq!(
        o.echo,
        "\
.draft 2025-01.csv 2025-02.csv 2025-03.csv 2025-04.csv 2025-05.csv 2025-06.csv \
2025-07.csv 2025-08.csv 2025-11.csv 2025-12.csv 2025-09.xlsx 2025-10.xlsx"
    );
    assert!(o.text.contains("CREATE TABLE dataset") && o.text.contains("in 11 of 12 file(s)"));
    let Payload::Drafted { wrote, .. } = o.payload else { panic!() };
    assert!(wrote.is_none());

    let o = s.run(".draft 2025-*.csv --to mine.tdy.sql", None).await;
    assert!(o.ok && d.path().join("mine.tdy.sql").exists());
    assert!(o.text.contains("wrote mine.tdy.sql"));
    let o = s.run(".draft 2025-*.csv --to mine.tdy.sql", None).await;
    assert!(!o.ok && o.text.contains("exists"));

    let o = s.run(".draft nothing-*.csv", None).await;
    assert!(!o.ok && o.text.contains("no file matches"));
}

#[tokio::test]
async fn fit_reports_refusals_writes_no_lock_and_then_fits_the_fixed_target() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".fit sales.tdy.sql", None).await;
    assert!(!o.ok);
    let Payload::Fitted(r) = &o.payload else { panic!("{:?}", o.payload) };
    assert_eq!((r.fitted, r.failed), (9, 3));
    assert!(r.lock_written.is_none());
    assert!(o.text.contains("9 of 12 file(s) fit `sales`"));
    assert!(o.text.ends_with("Error: 3 file(s) cannot reach the declared schema; no lock written. Fix them, exclude them, or widen the target.\n"));
    assert!(!d.path().join("sales.tdy.lock").exists());

    std::fs::copy(corpus().join("sales_ok.tdy.sql"), d.path().join("sales_ok.tdy.sql")).unwrap();
    let o = s.run(".fit sales_ok.tdy.sql", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(d.path().join("sales_ok.tdy.lock").exists());

    // One file against the target: the fit-one text path.
    let o = s.run(".fit sales.tdy.sql 2025-07.csv", None).await;
    assert!(!o.ok && o.text.contains("cannot reach `sales`"));
    let o = s.run(".fit sales.tdy.sql 2025-01.csv --dry-run", None).await;
    assert!(o.ok && o.text.contains("--dry-run: nothing written"));
}

#[tokio::test]
async fn check_schema_config_edit() {
    let d = pile();
    let mut s = session(d.path()).await;
    let o = s.run(".check sales.tdy.sql", None).await;
    assert!(o.ok && o.text.contains("nothing to check"));
    let o = s.run(".schema", None).await;
    assert!(o.ok && o.text.trim_start().starts_with('{'));
    let o = s.run(".config init", None).await;
    assert!(o.ok, "{}", o.text);
    // The real sample config, not a paraphrase of it: `[inference]` is its
    // first section and `backend = "local"` the setting people come for.
    assert!(o.text.contains("[inference]") && o.text.contains("backend = \"local\""), "{}", o.text);
    let o = s.run(".edit sales.tdy.sql", None).await;
    assert!(o.ok);
    assert!(matches!(o.payload, Payload::Edit(ref p) if p.ends_with("sales.tdy.sql")));
}

#[tokio::test]
async fn check_against_expands_a_glob() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-11.csv --no-llm", None).await;
    s.run(".sniff 2025-12.csv --no-llm", None).await;
    // No shell sits in front of this console (design §3): `--against` must
    // be glob-expanded here, the same way `confine_command_paths` already
    // expands it for the confinement pre-check — otherwise this would reach
    // `check_text` as a literal file named `2025-1*.csv` and fail as
    // "does not exist" instead of naming the two real, sniffed files.
    let o = s.run(".check sales.tdy.sql --against 2025-1*.csv", None).await;
    assert!(o.text.contains("2025-11.csv") && o.text.contains("2025-12.csv"), "{}", o.text);
    assert!(!o.text.contains("does not exist"), "{}", o.text);
}

#[tokio::test]
async fn sql_runs_when_the_statement_ends_and_spans_lines() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run("SELECT count(*) AS n, sum(betrag) AS total", None).await;
    assert!(o.ok && matches!(o.payload, Payload::Continue) && s.sql_pending());
    let o = s.run("FROM messy('2025-01.csv');", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(!s.sql_pending());
    assert_eq!(o.echo, "SELECT count(*) AS n, sum(betrag) AS total\nFROM messy('2025-01.csv');");
    let Payload::Query(t) = o.payload else { panic!() };
    assert_eq!(t.columns, ["n", "total"]);
    assert_eq!(t.rows, [["4", "4460.00"]]);
    assert!(o.text.contains("| 4 ") && o.text.contains("4460.00"));

    // A dot-command discards a pending statement, out loud.
    s.run("SELECT 1", None).await;
    let o = s.run(".ls", None).await;
    assert!(o.ok && o.text.starts_with("note: discarded incomplete statement"));
    assert!(!s.sql_pending());

    // ... and so does a dot-command that does not even parse: the buffer is
    // gone either way, so the note is owed on the error path too.
    s.run("SELECT 2", None).await;
    let o = s.run(".nope", None).await;
    assert!(!o.ok, "{}", o.text);
    assert_eq!(
        o.text,
        "note: discarded incomplete statement: SELECT 2\nError: unknown command `.nope` — `.help` lists them\n"
    );
    assert!(!s.sql_pending());

    // A bad statement is an error outcome, not a crash.
    let o = s.run("SELEKT 1;", None).await;
    assert!(!o.ok && o.text.starts_with("Error: "));
}

/// `.abort` discards a buffered statement out loud, exactly like any other
/// dot-command interrupting one — and says "nothing pending" rather than
/// stacking a second note when there was nothing to discard. A line right
/// after it must not still see the discard prefix (the buffer is gone).
#[tokio::test]
async fn abort_discards_pending_sql() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run("SELECT 1", None).await;
    assert!(s.sql_pending());
    let o = s.run(".abort", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains("discarded"), "{}", o.text);
    assert!(!s.sql_pending());

    let o = s.run(".ls", None).await;
    assert!(!o.text.starts_with("note: discarded"), "{}", o.text);

    let o = s.run(".abort", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains("nothing pending"), "{}", o.text);
}

#[tokio::test]
async fn output_routes_the_next_result_to_a_file() {
    let d = pile();
    let mut s = session(d.path()).await;
    s.run(".sniff 2025-01.csv --no-llm", None).await;
    let o = s.run(".output jan.csv", None).await;
    assert!(o.ok && o.text.contains("next result -> jan.csv"));
    let o = s.run("SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region;", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(o.text.contains("wrote 4 row(s) to jan.csv"));
    let written = std::fs::read_to_string(d.path().join("jan.csv")).unwrap();
    assert!(written.starts_with("region,betrag\n"));
    // The route is consumed.
    let o = s.run("SELECT 1 AS one;", None).await;
    assert!(o.text.contains("| one |"));
    // Refuses to overwrite without --force.
    let o = s.run(".output jan.csv", None).await;
    assert!(!o.ok && o.text.contains("exists"));
    assert!(s.run(".output jan.csv --force", None).await.ok);
    assert!(s.run(".output", None).await.ok); // back to the screen
}

/// A compile-time check, not a runtime one: `CWD_LOCK` used to be a
/// `std::sync::Mutex`, and holding its guard across the `.await` inside
/// `run_sql` made `Session::run`'s returned future `!Send` — which fails to
/// compile under `tokio::spawn`, exactly how the workbench slice is
/// designed to drive it. `assert_send` only typechecks; if this file
/// compiles, the future is `Send`.
#[tokio::test]
async fn session_run_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let d = pile();
    let mut s = session(d.path()).await;
    assert_send(s.run(".help", None));
    s.run(".help", None).await; // and still drive it once
}

const RAPPEN_SIDECAR: &str = r#"
# hand-written: Betrag Rp. is Rappen; shift the point two places left.
spec_version = 1
[source]
path = "2025-07.csv"
blake3 = "REPLACED"
bytes = 0
[provenance]
method = "manual"
tool_version = "test"
created_at = "2026-01-01T00:00:00Z"
[spec]
confidence = 1.0
notes = []
[spec.extraction]
format = "delimited"
delimiter = ";"
[[spec.transforms]]
op = "promote_header"
rows = 1
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
[spec.columns.dtype]
type = "decimal"
precision = 14
scale = 2
[spec.columns.parse]
decimal_shift = -2
"#;

fn write_rappen_sidecar(dir: &Path) {
    let file = dir.join("2025-07.csv");
    let (hash, bytes) = tdy::sidecar::hash_file(&file).unwrap();
    let toml = RAPPEN_SIDECAR.replace("blake3 = \"REPLACED\"", &format!("blake3 = \"{hash}\""))
        .replace("bytes = 0", &format!("bytes = {bytes}"));
    std::fs::write(dir.join("2025-07.csv.tdy.toml"), toml).unwrap();
}

#[tokio::test]
async fn accept_shows_evidence_first_and_accepts_only_on_repeat() {
    let d = pile();
    write_rappen_sidecar(d.path());
    let mut s = session(d.path()).await;

    // The pile fit leaves 2025-07 waiting on review (manual spec, decimal_shift).
    let o = s.run(".fit sales.tdy.sql", None).await;
    let Payload::Fitted(r) = &o.payload else { panic!() };
    let m07 = r.members.iter().find(|m| m.path == "2025-07.csv").unwrap();
    assert!(m07.review.is_some() && !m07.accepted, "{m07:?}");

    // Step one: evidence, nothing written.
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(o.ok, "{}", o.text);
    let Payload::Evidence { rows, .. } = &o.payload else { panic!("{:?}", o.payload) };
    assert!(!rows.is_empty());
    assert!(o.text.contains("170000") && o.text.contains("1700.00"));
    assert!(o.text.contains("run `.accept sales.tdy.sql 2025-07.csv` again to accept"));
    assert!(s.pending_accept().is_some());

    // Any other command in between resets to step one.
    s.run(".ls", None).await;
    assert!(s.pending_accept().is_none());
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(matches!(o.payload, Payload::Evidence { .. }));
    assert!(s.pending_accept().is_some());

    // A blank line is still "another line in between" — bare Enter must not
    // let the next identical `.accept` be mistaken for the repeat.
    s.run("", None).await;
    assert!(s.pending_accept().is_none());
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(matches!(o.payload, Payload::Evidence { .. }));
    assert!(s.pending_accept().is_some());
    s.run("   ", None).await;
    assert!(s.pending_accept().is_none());
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(matches!(o.payload, Payload::Evidence { .. }));
    assert!(s.pending_accept().is_some());

    // A bad TARGET still bails before the `Accept` arm's own confinement
    // check, in `confine_command_paths` — but must still consume a stale
    // marker from before, or the next good `.accept` line would be
    // mistaken for a repeat of THIS failed one rather than starting fresh.
    let o = s.run(".accept nonexistent.tdy.sql 2025-07.csv", None).await;
    assert!(!o.ok, "{}", o.text);
    assert!(s.pending_accept().is_none());
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    assert!(matches!(o.payload, Payload::Evidence { .. }));

    // Step two: the same line again performs the acceptance.
    let o = s.run(".accept sales.tdy.sql 2025-07.csv", None).await;
    let Payload::Fitted(r) = &o.payload else { panic!("{:?}", o.payload) };
    let m07 = r.members.iter().find(|m| m.path == "2025-07.csv").unwrap();
    assert!(m07.accepted, "{m07:?}");
    assert!(s.pending_accept().is_none());

    // A member with nothing to review is refused, not silently accepted.
    let o = s.run(".accept sales.tdy.sql 2025-01.csv", None).await;
    assert!(!o.ok && o.text.contains("nothing to accept"));
}

#[tokio::test]
async fn fit_text_equals_the_binary_stdout_plus_its_error_line() {
    let d = pile();
    let cli = tdy(d.path(), &["fit", "sales.tdy.sql"]);
    assert!(!cli.status.success());
    let stderr = String::from_utf8_lossy(&cli.stderr);
    let error_line = stderr.lines().find(|l| l.starts_with("Error: ")).expect("an Error line");
    let expected = format!("{}{error_line}\n", String::from_utf8_lossy(&cli.stdout));
    // Fresh copy so sidecars written by the CLI run do not change the text.
    let d2 = pile();
    let mut s = session(d2.path()).await;
    let o = s.run(".fit sales.tdy.sql", None).await;
    assert_eq!(o.text, expected);
}

#[tokio::test]
async fn query_text_equals_the_binary() {
    let d = pile();
    tdy(d.path(), &["sniff", "2025-01.csv", "--no-llm"]);
    let cli = tdy(d.path(), &["query", "SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region"]);
    assert!(cli.status.success());
    let mut s = session(d.path()).await;
    let o = s.run("SELECT region, betrag FROM messy('2025-01.csv') ORDER BY region;", None).await;
    assert_eq!(o.text, String::from_utf8_lossy(&cli.stdout));
}

#[tokio::test]
async fn draft_text_equals_the_binary() {
    let d = pile();
    let cli = tdy(d.path(), &["draft", "2025-01.csv", "2025-02.csv", "2025-12.csv"]);
    let mut s = session(d.path()).await;
    let o = s.run(".draft 2025-01.csv 2025-02.csv 2025-12.csv", None).await;
    assert_eq!(o.text, String::from_utf8_lossy(&cli.stdout));
}

/// `.cd` must move the SQL surface too, or the console has two path
/// universes and the query answers from the wrong one.
///
/// The experiment is a decoy: the same file name at the root and in a
/// subdirectory, carrying different numbers. After `.cd sub`, `.ls` and
/// `.sniff` speak about `sub/x.csv`; a relative `messy('x.csv')` used to be
/// joined onto the *root* instead and answered 100.00 — sub's file sniffed
/// and sidecar'd a line earlier, the root's file read and summed, exit 0, no
/// warning. That is the silent wrong value the whole project is built to
/// refuse, so the sum is asserted exactly, and so is the absence of a
/// sidecar beside the decoy: nothing may have read it at all.
#[tokio::test]
async fn cd_moves_the_sql_surface_not_only_the_dot_commands() {
    let d = tempfile::tempdir().unwrap();
    let sub = d.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(d.path().join("x.csv"), "Datum;Region;Betrag\n31.01.2025;Ost;100.00\n").unwrap();
    std::fs::write(sub.join("x.csv"), "Datum;Region;Betrag\n31.01.2025;Ost;999.00\n").unwrap();
    std::fs::write(sub.join("only_in_sub.csv"), "Datum;Region;Betrag\n31.01.2025;Ost;7.00\n").unwrap();

    let mut s = session(d.path()).await;
    let o = s.run(".cd sub", None).await;
    assert!(o.ok, "{}", o.text);

    let o = s.run(".sniff x.csv --no-llm", None).await;
    assert!(o.ok, "{}", o.text);
    assert!(sub.join("x.csv.tdy.toml").exists(), "sniffed the wrong x.csv");

    let o = s.run("SELECT sum(betrag) AS total FROM messy('x.csv');", None).await;
    assert!(o.ok, "{}", o.text);
    let Payload::Query(t) = &o.payload else { panic!("{}", o.text) };
    assert_eq!(t.rows, [["999.00"]], "read the root's decoy, not sub's file: {}", o.text);

    // The decoy was never opened: no sidecar was written beside it, so the
    // query did not quietly sniff it on the fly either.
    assert!(!d.path().join("x.csv.tdy.toml").exists(), "the query touched the root's decoy");

    // And a file that exists *only* under the session's cwd is reachable at
    // all — joined onto the root it was "does not exist under ..." while
    // `.ls` was listing it.
    let o = s.run("SELECT sum(betrag) AS total FROM messy('only_in_sub.csv');", None).await;
    assert!(o.ok, "{}", o.text);
    let Payload::Query(t) = &o.payload else { panic!("{}", o.text) };
    assert_eq!(t.rows, [["7.00"]], "{}", o.text);

    // Confinement still holds: the root is still the whole of what is
    // allowed, and `base` cannot widen it.
    let o = s.run("SELECT 1 FROM messy('/etc/passwd');", None).await;
    assert!(!o.ok && o.text.contains("outside"), "{}", o.text);
}
