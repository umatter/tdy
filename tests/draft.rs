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
    // Two columns on purpose: a one-column all-text file is genuinely
    // ambiguous (its first row could be a header or a value), and since the
    // 2026-09-03 corpus audit tdy refuses to guess — see header_verdict's
    // width > 1 guards. The conflict under test is `menge` TEXT vs BIGINT,
    // which two columns exercise exactly as well.
    std::fs::write(&c, "id,menge\n1,viel\n2,wenig\n3,etwas\n").unwrap();
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
    // A scalar document: nothing tabular. (A lone object used to stand in
    // here; it is a record now, and drafts as one.)
    std::fs::write(&junk, "\"not records\"").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["draft", junk.to_str().unwrap()])
        .output()
        .expect("run tdy");
    assert!(!out.status.success(), "an undraftable pile must fail loudly");
}

/// A directory is not a dataset. When the pile's files share almost no
/// columns, the union target would refuse everything — mechanically correct,
/// humanly useless — so the draft says what it can see: this is several
/// shapes, and names the groups.
#[test]
fn a_pile_of_unrelated_files_is_called_out_as_several_datasets() {
    let dir = tempfile::tempdir().unwrap();
    let cars = dir.path().join("cars.csv");
    let movies = dir.path().join("movies.csv");
    let more_cars = dir.path().join("cars2.csv");
    std::fs::write(&cars, "car_id,make,ps\n1,VW,110\n2,BMW,190\n").unwrap();
    std::fs::write(&more_cars, "car_id,make,ps\n3,Opel,90\n").unwrap();
    std::fs::write(&movies, "title,rating\nHeat,9\nTaxi Driver,10\n").unwrap();
    let sql = tdy::draft::draft_target(&[cars, more_cars, movies], Limits::default()).unwrap();
    Target::parse(&sql).unwrap_or_else(|e| panic!("draft must still parse:\n{sql}\n{e:#}"));
    assert!(sql.contains("do not look like ONE dataset"), "{sql}");
    assert!(sql.contains("group 1: cars.csv, cars2.csv"), "{sql}");
    assert!(sql.contains("group 2: movies.csv"), "{sql}");

    // And a homogeneous pile carries no such note.
    let sql = draft_of(&["2025-01.csv", "2025-02.csv"]);
    assert!(!sql.contains("do not look like ONE dataset"), "{sql}");
}

/// A file holding two stacked tables (a detail block, then a summary block)
/// is drafted block by block, not as one file: each block is sniffed on its
/// own, so the summary block's `total` column shows up as its own drafted
/// column, attributed to `#2` — never silently folded into (or dropped by)
/// a single whole-file sniff that would see only the first block's shape.
///
/// One input file is one physical file: the header must say so (`from 1
/// file(s)`, not "2" for its two blocks), and a single file's own stacked
/// blocks must never trip the "these files do not look like ONE dataset"
/// heterogeneity note — that note is for genuinely different files sharing a
/// directory, not for one file's own internal structure. `total`'s `#2`
/// attribution has to come from its own per-column comment, not as a
/// side-effect of a spurious grouping note naming `regions_summary.csv#2`
/// as if it were a real, separately-draftable path.
#[test]
fn a_file_with_stacked_blocks_drafts_each_blocks_columns_and_names_the_block() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/regions_summary.csv");
    let ddl = tdy::draft::draft_target(&[p], Limits::default()).unwrap();
    assert!(ddl.contains("from 1 file(s)"), "one physical file was given: {ddl}");
    assert!(
        !ddl.contains("do not look like ONE dataset"),
        "one file's own stacked blocks are not a heterogeneous pile: {ddl}",
    );
    let total_line = ddl.lines().find(|l| l.trim_start().starts_with("total")).unwrap_or_else(|| panic!("no `total` column line: {ddl}"));
    assert!(total_line.contains("#2"), "`total` is attributed to its block by its own comment: {total_line}");
    assert!(Target::parse(&ddl).is_ok(), "{ddl}");
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn column_line<'a>(ddl: &'a str, name: &str) -> &'a str {
    ddl.lines()
        .find(|l| l.trim_start().starts_with(&format!("{name} ")))
        .unwrap_or_else(|| panic!("no `{name}` column line:\n{ddl}"))
}

/// A currency-formatted cell holding a computed float types as a DECIMAL
/// whose scale is the sample's widest noise (15 here), and a later row can
/// carry one place more. The fit refuses that value unless rounding is
/// declared — correctly — so the draft, which reproduces the sniffer's
/// scale, must declare the rounding with it, and say why. Scale 2 money in
/// the same file is left alone.
#[test]
fn a_currency_formatted_float_column_drafts_with_rounding_declared() {
    let ddl = tdy::draft::draft_target(&[fixture("draft_float_money.xlsx")], Limits::default()).unwrap();
    Target::parse(&ddl).unwrap_or_else(|e| panic!("draft must parse:\n{ddl}\n{e:#}"));
    let energy = column_line(&ddl, "energy_value_2020_mwh");
    assert!(energy.contains("DECIMAL(38,15)"), "{energy}");
    assert!(energy.contains("round = 'half_away'"), "{energy}");
    assert!(energy.contains("float noise"), "the comment says why: {energy}");
    assert!(energy.contains("DOUBLE"), "the comment names the other edit: {energy}");
    let capacity = column_line(&ddl, "capacity_value_2020_mwh");
    assert!(capacity.contains("DECIMAL(38,2)"), "{capacity}");
    assert!(!capacity.contains("round ="), "money's own places declare nothing: {capacity}");
}

/// The draft's promise on that file: unedited, it fits, the lock is
/// written, and the one 16-place value is rounded half away at scale 15 —
/// the sum is the generator's exact ground truth.
#[test]
fn the_unedited_draft_fits_a_currency_formatted_float_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("draft_float_money.xlsx"), dir.path().join("draft_float_money.xlsx")).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(args)
            .current_dir(dir.path())
            .env("TDY_BACKEND", "none")
            .output()
            .expect("run tdy")
    };
    let out = run(&["draft", "draft_float_money.xlsx"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::write(dir.path().join("d.tdy.sql"), &out.stdout).unwrap();

    let out = run(&["fit", "d.tdy.sql"]);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "the unedited draft must fit:\n{text}");
    assert!(!text.contains("GAP"), "{text}");
    assert!(dir.path().join("d.tdy.lock").exists(), "no lock written:\n{text}");

    let out = run(&["query", "SELECT sum(energy_value_2020_mwh) AS e, sum(capacity_value_2020_mwh) AS c FROM dataset('d.tdy.sql')"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("23357.561107253265903"), "ground truth at scale 15:\n{text}");
    assert!(text.contains("1040.00"), "{text}");
}

/// The control: money at its own few places (scale <= 6, the sniffer's
/// non-currency cap) drafts exactly as before, with no rounding declared.
/// `xl_money_siblings.xlsx` carries both kinds side by side: `added` is
/// money at 2 places, the `amount_*` columns carry the source file's baked-in
/// noise at 8-9 places — the rule is per column, by scale.
#[test]
fn small_scale_money_drafts_without_rounding() {
    let ddl = tdy::draft::draft_target(&[fixture("xl_money_offset_a.xlsx")], Limits::default()).unwrap();
    assert!(ddl.contains("DECIMAL(38,2)"), "{ddl}");
    assert!(!ddl.contains("round ="), "{ddl}");

    let ddl = tdy::draft::draft_target(&[fixture("xl_money_siblings.xlsx")], Limits::default()).unwrap();
    Target::parse(&ddl).unwrap_or_else(|e| panic!("draft must parse:\n{ddl}\n{e:#}"));
    assert!(!column_line(&ddl, "added").contains("round ="), "{ddl}");
    for c in ["amount_a", "amount_b", "amount_c"] {
        assert!(column_line(&ddl, c).contains("round = 'half_away'"), "{ddl}");
    }
}

/// Types merge to the widest scale, so one noisy file makes the merged
/// column declare the rounding; the comment then names the file the scale
/// came from, since the others carry money's own places.
#[test]
fn a_noisy_scale_from_one_file_of_several_is_attributed() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("other.csv");
    std::fs::write(
        &csv,
        "Site,Energy Value 2020$/MWh,Capacity Value 2020$/MWh\nA,12.50,1.25\nB,13.75,2.00\nC,14.10,1.50\n",
    )
    .unwrap();
    let ddl = tdy::draft::draft_target(&[fixture("draft_float_money.xlsx"), csv], Limits::default()).unwrap();
    Target::parse(&ddl).unwrap_or_else(|e| panic!("draft must parse:\n{ddl}\n{e:#}"));
    let energy = column_line(&ddl, "energy_value_2020_mwh");
    assert!(energy.contains("DECIMAL(38,15)") && energy.contains("round = 'half_away'"), "{energy}");
    assert!(energy.contains("scale 15 (from draft_float_money.xlsx)"), "{energy}");
    let capacity = column_line(&ddl, "capacity_value_2020_mwh");
    assert!(!capacity.contains("round ="), "{capacity}");
    // A scale every file carries names no file.
    let ddl = tdy::draft::draft_target(&[fixture("draft_float_money.xlsx")], Limits::default()).unwrap();
    assert!(!column_line(&ddl, "energy_value_2020_mwh").contains("(from "), "{ddl}");
}

/// The glob is relative only when the files sit in the directory the draft
/// is run from or below it; anywhere else it is absolute. A ladder of `..`
/// up to a shared prefix such as `/tmp` was relative to the current
/// directory, and from a target written beside the data it named no file.
#[test]
fn a_draft_of_files_outside_the_current_directory_writes_an_absolute_glob() {
    let here = tempfile::TempDir::new().unwrap();
    let there = tempfile::TempDir::new().unwrap();
    let rows = "Datum;Region;Betrag\n05.01.2025;Ost;190.00\n12.01.2025;West;200.00\n";
    std::fs::write(there.path().join("a.csv"), rows).unwrap();
    std::fs::create_dir(here.path().join("sub")).unwrap();
    std::fs::write(here.path().join("sub").join("b.csv"), rows).unwrap();
    let run = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).current_dir(cwd).env("TDY_BACKEND", "none").output().unwrap()
    };

    let a = there.path().join("a.csv");
    let out = run(here.path(), &["draft", a.to_str().unwrap()]);
    let ddl = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{ddl}");
    let abs = there.path().canonicalize().unwrap();
    assert!(ddl.contains(&format!("files = '{}/*.csv'", abs.display())), "{ddl}");
    // Unedited, written into yet another directory, it still names the file.
    let elsewhere = tempfile::TempDir::new().unwrap();
    let t = elsewhere.path().join("t.tdy.sql");
    std::fs::write(&t, &ddl).unwrap();
    let out = run(here.path(), &["fit", t.to_str().unwrap()]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    // Below the current directory, absolute or relative argument alike, the
    // glob stays relative.
    let b = here.path().join("sub").join("b.csv");
    for arg in [b.to_str().unwrap(), "sub/b.csv"] {
        let out = run(here.path(), &["draft", arg]);
        assert!(String::from_utf8_lossy(&out.stdout).contains("files = 'sub/*.csv'"), "{}", String::from_utf8_lossy(&out.stdout));
    }
    // And a relative path that climbs out is absolute too.
    let out = run(&here.path().join("sub"), &["draft", "../sub/b.csv"]);
    let sub = here.path().join("sub").canonicalize().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("files = '*.csv'"), "{}", String::from_utf8_lossy(&out.stdout));
    let out = run(&here.path().join("sub"), &["draft", a.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("files = '{}/*.csv'", abs.display())), "{sub:?}");
}
