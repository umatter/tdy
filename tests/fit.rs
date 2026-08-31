//! `tdy fit`: planning a spec onto a declared target.
//!
//! The corpus is `testdata/drifting_exports/` — twelve monthly exports that
//! disagree with each other, and one SQL target declaring what they should all
//! become. Nine must fit. **Three must be refused**, and those three are the
//! point: each is a different way for a tool to be quietly wrong, and a
//! planner that "helpfully" landed any of them would produce a number that is
//! well-typed, raises no error, and is incorrect.
//!
//! The arithmetic is checkable by hand — see the generator's docstring — so
//! this file asserts the total rather than only the shape. A planner that
//! bound the wrong column would still conform, still execute, and fail here.

use std::path::{Path, PathBuf};

use datafusion::arrow::array::Array;
use tdy::config::Limits;
use tdy::conform::conforms;
use tdy::fit::{fit, FitError, Gap};
use tdy::target::Target;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

fn target() -> Target {
    Target::load(&corpus().join("sales.tdy.sql")).expect("the corpus target must parse")
}

/// Every file the corpus says must fit, and the three it says must not.
const FITTABLE: &[&str] = &[
    "2025-01.csv",
    "2025-02.csv",
    "2025-03.csv",
    "2025-04.csv",
    "2025-05.csv",
    "2025-06.csv",
    "2025-09.xlsx",
    "2025-10.xlsx",
    "2025-12.csv",
];

/// Nine files, three formats, two date conventions, two languages, one
/// declared schema — and no hand-written spec anywhere.
#[test]
fn the_ordinary_members_of_the_corpus_fit() {
    let t = target();
    for name in FITTABLE {
        let p = corpus().join(name);
        let fitted = match fit(&p, &t, Limits::default()) {
            Ok(f) => f,
            Err(e) => panic!("{name} should fit but did not:\n{e}"),
        };
        // A fit that did not conform would be a bug in the gate, not a gap.
        assert!(
            conforms(&fitted.spec, &t).is_ok(),
            "{name}: fit returned a spec that does not conform"
        );
        assert_eq!(fitted.spec.columns.len(), 3, "{name}");
        // A fitted spec is not a guess and must not carry a confidence.
        assert!(fitted.spec.confidence.is_none(), "{name}: a fitted spec claimed a confidence");
    }
}

/// The mapping each file needed, stated exactly. This is what "the tool
/// figures out how to get there" has to mean concretely: different header
/// names, different date formats, different numeric conventions, one schema.
#[test]
fn the_planner_picks_the_right_source_column_and_format_per_file() {
    let t = target();
    let expect: &[(&str, [&str; 3], &str)] = &[
        // file, [month<-, region<-, amount_chf<-], date format
        ("2025-01.csv", ["Datum", "Region", "Betrag"], "%d.%m.%Y"),
        // A merged band above the real header, and the amount spelt differently.
        ("2025-09.xlsx", ["Datum", "Region", "Betrag CHF"], "%d.%m.%Y"),
        // An English export with ISO dates — which must NOT be pruned by the
        // dataset's `date_order = 'dmy'`, because an ISO date was never
        // ambiguous with a day-first one.
        ("2025-10.xlsx", ["Date", "Region", "Amount"], "%Y-%m-%d"),
    ];

    for (name, sources, fmt) in expect {
        let fitted = fit(&corpus().join(name), &t, Limits::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let got: Vec<&str> = fitted.spec.columns.iter().map(|c| c.source_name()).collect();
        assert_eq!(&got, sources, "{name}: bound the wrong source columns");

        let month = &fitted.spec.columns[0];
        match &month.dtype {
            tdy::spec::DType::Date { format } => {
                assert_eq!(format, fmt, "{name}: wrong date format")
            }
            other => panic!("{name}: month is {other:?}, not a date"),
        }
    }
}

fn gap_of(name: &str) -> Vec<Gap> {
    let t = target();
    match fit(&corpus().join(name), &t, Limits::default()) {
        Err(FitError::Gaps(g)) => g,
        Ok(_) => panic!("{name} fitted, but the corpus says it must be refused"),
        Err(e) => panic!("{name}: expected gaps, got {e}"),
    }
}

/// THE UNIT TRAP. `Betrag Rp.` holds integer Rappen — the values parse, the
/// type checks, and binding it to `amount_chf` would be out by a factor of a
/// hundred with the error invisible in any single row. Nothing declares that
/// column, so nothing may bind it.
#[test]
fn the_rappen_file_is_refused_because_nothing_declares_its_amount_column() {
    let gaps = gap_of("2025-07.csv");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    match &gaps[0] {
        Gap::NoCandidate { column, header, .. } => {
            assert_eq!(column, "amount_chf");
            assert!(
                header.iter().any(|h| h == "Betrag Rp."),
                "the message does not show the column that is there: {header:?}"
            );
        }
        other => panic!("expected NoCandidate, got {other:?}"),
    }
    // And the message tells the user what to do about it.
    assert!(gaps[0].message().contains("OPTIONS(matches"), "{}", gaps[0].message());
}

/// THE AMBIGUITY TRAP, and the one that is easiest to get wrong: the file has
/// two columns literally named `Betrag` (net and gross). `dedupe_names`
/// renames the second to `Betrag_2` so a spec can address it — which would let
/// a planner match exactly one candidate and bind it silently. Matching is
/// therefore done against the file's own spelling, where both are still
/// `Betrag`, so the collision is visible and refused.
#[test]
fn two_columns_with_the_same_name_are_ambiguous_not_first_wins() {
    let gaps = gap_of("2025-08.csv");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    match &gaps[0] {
        Gap::Ambiguous { column, candidates } => {
            assert_eq!(column, "amount_chf");
            assert_eq!(candidates.len(), 2, "{candidates:?}");
            assert!(candidates.iter().all(|(_, n)| n == "Betrag"), "{candidates:?}");
            // Positions, so the user can tell them apart at all.
            assert_ne!(candidates[0].0, candidates[1].0);
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    let m = gaps[0].message();
    assert!(m.contains("column 3") && m.contains("column 4"), "{m}");
}

/// THE PARTIAL EXPORT. There is no plan that reaches the target, so there is
/// no plan — not a load with `region` nulled. A dataset quietly short one
/// column is the aggregate-laundering failure the whole design refuses.
#[test]
fn a_file_missing_a_declared_column_is_refused_not_null_filled() {
    let gaps = gap_of("2025-11.csv");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0].column(), "region");
    assert!(matches!(gaps[0], Gap::NoCandidate { .. }));
}

/// The arithmetic, over the whole corpus. Shape is not enough: a planner that
/// bound `Rabatt` to `amount_chf` would conform, execute, and be wrong. The
/// generator states this total and computes it independently.
#[test]
fn the_fitted_corpus_sums_to_the_declared_ground_truth() {
    let t = target();
    let mut total = 0i128;
    let mut rows = 0usize;
    for name in FITTABLE {
        let p = corpus().join(name);
        let fitted = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        let batch = tdy::provider::spec_to_batch(&fitted.spec, &p)
            .unwrap_or_else(|e| panic!("{name}: executing the fitted spec: {e:#}"));

        let col = batch
            .column(2)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
            .unwrap_or_else(|| panic!("{name}: amount_chf is not an exact decimal"));
        for i in 0..col.len() {
            assert!(!col.is_null(i), "{name} row {i}: a NOT NULL amount was null");
            total += col.value(i);
        }
        rows += batch.num_rows();
    }
    // 57'340.00, held as Decimal128(14,2) so the total is exact.
    assert_eq!(rows, 36, "wrong number of rows across the corpus");
    assert_eq!(total, 5_734_000, "the corpus does not sum to 57340.00");
}

/// A declared `date_order` resolves a real conflict; it does not prune the
/// candidate list. Pruning threw away `%Y-%m-%d` on a dataset declared 'dmy'
/// and made an ordinary ISO export unfittable, even though an ISO date can
/// never be confused with a day-first one.
#[test]
fn date_order_resolves_ambiguity_without_excluding_unambiguous_formats() {
    let t = target();
    assert_eq!(t.date_order, Some(tdy::target::DateOrder::Dmy));

    // ISO, under a 'dmy' dataset: fits, with the ISO format.
    let iso = fit(&corpus().join("2025-10.xlsx"), &t, Limits::default()).unwrap();
    assert!(matches!(
        &iso.spec.columns[0].dtype,
        tdy::spec::DType::Date { format } if format == "%Y-%m-%d"
    ));

    // Day-first, under the same dataset: fits, with the day-first format —
    // and its 31.01.2025 could only ever be day-first anyway.
    let dmy = fit(&corpus().join("2025-01.csv"), &t, Limits::default()).unwrap();
    assert!(matches!(
        &dmy.spec.columns[0].dtype,
        tdy::spec::DType::Date { format } if format == "%d.%m.%Y"
    ));
}

/// Genuinely ambiguous dates — every day-of-month under 13, so day-first and
/// month-first both parse and mean different things — must be refused when
/// nothing settles them, and accepted once the dataset declares its
/// convention. A `Date32` holding the wrong month is exactly the plausible
/// wrong number this project exists to refuse.
#[test]
fn a_genuinely_ambiguous_date_is_refused_until_the_convention_is_declared() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("amb.csv");
    std::fs::write(&p, "d;v\n03/04/2025;1\n05/06/2025;2\n07/08/2025;3\n").unwrap();

    let undeclared = Target::parse(
        "CREATE TABLE t (d DATE NOT NULL, v BIGINT NOT NULL) WITH (files = 'amb.csv')",
    )
    .unwrap();
    match fit(&p, &undeclared, Limits::default()) {
        Err(FitError::Gaps(g)) => {
            assert_eq!(g.len(), 1, "{g:?}");
            match &g[0] {
                Gap::AmbiguousFormat { column, formats, .. } => {
                    assert_eq!(column, "d");
                    assert!(formats.len() >= 2, "{formats:?}");
                }
                other => panic!("expected AmbiguousFormat, got {other:?}"),
            }
            assert!(g[0].message().contains("date_order"), "{}", g[0].message());
        }
        Ok(f) => panic!(
            "an ambiguous date was silently resolved to {:?}",
            f.spec.columns[0].dtype
        ),
        Err(e) => panic!("{e}"),
    }

    // Declared: the conflict is resolved, and to the declared reading.
    for (order, want) in [("dmy", "%d/%m/%Y"), ("mdy", "%m/%d/%Y")] {
        let sql = format!(
            "CREATE TABLE t (d DATE NOT NULL, v BIGINT NOT NULL) \
             WITH (files = 'amb.csv', date_order = '{order}')"
        );
        let t = Target::parse(&sql).unwrap();
        let f = fit(&p, &t, Limits::default())
            .unwrap_or_else(|e| panic!("date_order = {order} did not resolve it:\n{e}"));
        match &f.spec.columns[0].dtype {
            tdy::spec::DType::Date { format } => assert_eq!(format, want, "order {order}"),
            other => panic!("{other:?}"),
        }
    }
}

/// A column whose values cannot make the declared type is a gap naming the
/// column, not a panic and not a silent coercion.
#[test]
fn a_column_that_cannot_produce_the_declared_type_is_a_gap() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("t.csv");
    std::fs::write(&p, "id;amount\n1;not-a-number\n2;also-not\n").unwrap();

    let t = Target::parse(
        "CREATE TABLE t (id BIGINT NOT NULL, amount DECIMAL(14,2) NOT NULL) \
         WITH (files = 't.csv')",
    )
    .unwrap();
    match fit(&p, &t, Limits::default()) {
        Err(FitError::Gaps(g)) => {
            assert_eq!(g.len(), 1, "{g:?}");
            assert_eq!(g[0].column(), "amount");
            assert!(matches!(g[0], Gap::Untypable { .. }), "{:?}", g[0]);
        }
        other => panic!("expected a gap, got {other:?}"),
    }
}

/// Rounding is a value change, so it is said out loud rather than discovered
/// later in a total that is off by a rappen.
#[test]
fn rounding_to_the_declared_scale_is_reported() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("r.csv");
    std::fs::write(&p, "id;amount\n1;1.005\n2;2.994\n").unwrap();

    let t = Target::parse(
        "CREATE TABLE t (id BIGINT NOT NULL, amount DECIMAL(14,2) NOT NULL) \
         WITH (files = 'r.csv')",
    )
    .unwrap();
    let f = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        f.spec.notes.iter().any(|n| n.contains("rounded")),
        "rounding was not reported: {:?}",
        f.spec.notes
    );
}

/// Every gap in one pass. A user fixing a pile wants the whole list, not a
/// twelve-round game of whack-a-mole.
#[test]
fn every_gap_is_reported_not_just_the_first() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("t.csv");
    std::fs::write(&p, "x;y\n1;2\n").unwrap();

    let t = Target::parse(
        "CREATE TABLE t (a TEXT NOT NULL, b TEXT NOT NULL, c TEXT NOT NULL) \
         WITH (files = 't.csv')",
    )
    .unwrap();
    match fit(&p, &t, Limits::default()) {
        Err(FitError::Gaps(g)) => assert_eq!(g.len(), 3, "{g:?}"),
        other => panic!("expected three gaps, got {other:?}"),
    }
}

/// `tdy fit` writes a sidecar that `tdy check` then accepts, and `--dry-run`
/// writes nothing. The two commands have to agree, or the CI gate is checking
/// something the planner did not produce.
#[test]
fn the_cli_writes_a_sidecar_that_check_accepts() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("2025-01.csv");
    std::fs::copy(corpus().join("2025-01.csv"), &p).unwrap();
    let tgt = dir.path().join("sales.tdy.sql");
    std::fs::copy(corpus().join("sales.tdy.sql"), &tgt).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
            .args(args)
            .output()
            .expect("run tdy")
    };

    let dry = run(&["fit", tgt.to_str().unwrap(), p.to_str().unwrap(), "--dry-run"]);
    assert!(dry.status.success(), "{}", String::from_utf8_lossy(&dry.stderr));
    assert!(
        !tdy::sidecar::sidecar_path(&p).exists(),
        "--dry-run wrote a sidecar"
    );

    let real = run(&["fit", tgt.to_str().unwrap(), p.to_str().unwrap()]);
    assert!(real.status.success(), "{}", String::from_utf8_lossy(&real.stderr));
    assert!(tdy::sidecar::sidecar_path(&p).exists(), "fit wrote no sidecar");

    let check = run(&[
        "check",
        tgt.to_str().unwrap(),
        "--against",
        p.to_str().unwrap(),
    ]);
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(check.status.success(), "check rejected what fit produced:\n{text}");
    assert!(text.contains("CONFORMS"), "{text}");
}

/// A file the planner refuses must leave nothing behind. A half-written
/// sidecar would be worse than no sidecar: the next command would read it.
#[test]
fn a_refused_file_gets_no_sidecar() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("2025-11.csv");
    std::fs::copy(corpus().join("2025-11.csv"), &p).unwrap();
    let tgt = dir.path().join("sales.tdy.sql");
    std::fs::copy(corpus().join("sales.tdy.sql"), &tgt).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args(["fit", tgt.to_str().unwrap(), p.to_str().unwrap()])
        .output()
        .expect("run tdy");
    assert!(!out.status.success());
    assert!(
        !tdy::sidecar::sidecar_path(&p).exists(),
        "a refused file was given a sidecar anyway"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("region"), "{text}");
}
