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
use tdy::fit::{discover_sheets, fit, fit_sheet, FitError, Gap};
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
        // file, [month<-, region<-, amount<-], date format
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
/// type checks, and binding it to `amount` would be out by a factor of a
/// hundred with the error invisible in any single row. Nothing declares that
/// column, so nothing may bind it.
#[test]
fn the_rappen_file_is_refused_because_nothing_declares_its_amount_column() {
    let gaps = gap_of("2025-07.csv");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    match &gaps[0] {
        Gap::NoCandidate { column, header, .. } => {
            assert_eq!(column, "amount");
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
            assert_eq!(column, "amount");
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
/// bound `Rabatt` to `amount` would conform, execute, and be wrong. The
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
            .unwrap_or_else(|| panic!("{name}: amount is not an exact decimal"));
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

/// Rounding is a value change. A value with more fractional digits than the
/// declared scale is a gap naming the column and the value — unless the
/// target says `round = 'half_away'`, which is authorisation written in the
/// reviewed declaration, not a judgement the planner makes.
#[test]
fn more_fractional_digits_than_the_scale_is_a_gap_unless_the_target_declares_rounding() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("r.csv");
    std::fs::write(&p, "id;amount\n1;1.0056\n2;2.9942\n3;3.10\n").unwrap();

    let t = Target::parse(
        "CREATE TABLE t (id BIGINT NOT NULL, amount DECIMAL(14,2) NOT NULL) WITH (files = 'r.csv')",
    )
    .unwrap();
    let err = fit(&p, &t, Limits::default()).expect_err("rounding must not be silent");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps, got {err:?}") };
    let text = gaps.iter().map(|g| g.message()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("`amount`"), "{text}");
    assert!(text.contains("fractional digits"), "{text}");
    assert!(text.contains("1.0056"), "an offending value is named: {text}");
    assert!(text.contains("round = 'half_away'"), "the declaration that allows it is named: {text}");

    let t = Target::parse(
        "CREATE TABLE t (id BIGINT NOT NULL, amount DECIMAL(14,2) NOT NULL OPTIONS(round = 'half_away')) \
         WITH (files = 'r.csv')",
    )
    .unwrap();
    let f = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(f.spec.notes.iter().any(|n| n.contains("rounded")), "{:?}", f.spec.notes);
    let amount = f.spec.columns.iter().find(|c| c.name == "amount").unwrap();
    assert_eq!(amount.parse.round, Some(tdy::spec::Rounding::HalfAway));
    assert!(f.review.is_none(), "declared rounding is not a judgement: {:?}", f.review);
}

/// A fitted decimal column that did not declare rounding refuses at
/// execution too, so a value the probe never saw cannot round silently: the
/// whole-file verification finds it and the member is refused, naming the row.
#[test]
fn a_late_value_with_extra_digits_is_caught_by_verification_not_rounded() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("late.csv");
    let mut body = String::from("id;amount\n");
    for i in 1..=2600 {
        body.push_str(&format!("{i};{}.50\n", i % 100));
    }
    body.push_str("2601;7.125\n");
    std::fs::write(&p, body).unwrap();
    let t = Target::parse(
        "CREATE TABLE t (id BIGINT NOT NULL, amount DECIMAL(14,2) NOT NULL) WITH (files = 'late.csv')",
    )
    .unwrap();
    let err = fit(&p, &t, Limits::default()).expect_err("row 2601 has three fractional digits");
    let msg = format!("{err}");
    assert!(msg.contains("7.125") && msg.contains("row 2601"), "the value and its row are named: {msg}");
    assert!(msg.contains("past the sample"), "found by the whole-file verification, not the probe: {msg}");
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

// ---------------------------------------------------------------------------
// --propose
// ---------------------------------------------------------------------------

/// The friction the alias list creates is real: a target names what you want,
/// the files are somebody else's exports, and somebody has to bridge that
/// once. `propose` does the mechanical half — which of this file's unbound
/// columns *could* produce the declared type — and stops there.
///
/// It deliberately does not choose. A discount column parses as money exactly
/// as well as an amount does, and picking between them is the judgement this
/// tool does not make.
#[test]
fn propose_offers_type_compatible_columns_without_choosing() {
    let t = target();
    let props = tdy::fit::propose(&corpus().join("2025-07.csv"), &t, Limits::default())
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(props.len(), 1, "{props:?}");
    let p = &props[0];
    assert_eq!(p.column, "amount");
    assert_eq!(p.candidates.len(), 1, "{:?}", p.candidates);
    assert_eq!(p.candidates[0].0, "Betrag Rp.");

    // The remedy is pasteable, keeps the declared aliases, and does not repeat
    // the column's own name (the binder always tries that first).
    let existing = vec!["amount".to_string(), "Betrag".to_string()];
    let m = p.message(&existing);
    assert!(m.contains("OPTIONS(matches = 'Betrag, Betrag Rp.')"), "{m}");
    assert!(m.contains("not the same as correct"), "the caveat is missing:\n{m}");
}

/// A column another declared column already binds is not a candidate: the
/// proposal is about what is *free*, not about every column that happens to
/// parse.
#[test]
fn propose_ignores_columns_another_declared_column_already_binds() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("t.csv");
    // `menge` and `betrag` both parse as DECIMAL; `menge` is spoken for.
    std::fs::write(&p, "datum;menge;betrag\n31.01.2025;5;1234.50\n").unwrap();

    let t = Target::parse(
        "CREATE TABLE t (
           menge      DECIMAL(14,2) NOT NULL,
           amount DECIMAL(14,2) NOT NULL,
           datum      DATE          NOT NULL
         ) WITH (files = 't.csv', date_order = 'dmy')",
    )
    .unwrap();

    let props = tdy::fit::propose(&p, &t, Limits::default()).unwrap();
    assert_eq!(props.len(), 1, "{props:?}");
    assert_eq!(props[0].column, "amount");
    let names: Vec<&str> = props[0].candidates.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["betrag"], "a column already bound was offered: {names:?}");
}

/// Nothing to propose when nothing is unbound.
#[test]
fn propose_is_empty_when_the_file_already_fits() {
    let t = target();
    let props = tdy::fit::propose(&corpus().join("2025-01.csv"), &t, Limits::default()).unwrap();
    assert!(props.is_empty(), "{props:?}");
}

/// The suggestion actually works: pasting it in makes the file fit.
#[test]
fn the_proposed_alias_makes_the_file_fit() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("2025-07.csv");
    std::fs::copy(corpus().join("2025-07.csv"), &p).unwrap();

    // The Rappen file's amount column, declared. It fits — and reads the raw
    // integers, which is why a decimal_shift and a human are still needed
    // before it may join a dataset (see tests/dataset.rs).
    let t = Target::parse(
        "CREATE TABLE t (
           month      DATE          NOT NULL OPTIONS(matches = 'Datum'),
           region     TEXT          NOT NULL OPTIONS(matches = 'Region'),
           amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag, Betrag Rp.')
         ) WITH (files = '2025-07.csv', date_order = 'dmy')",
    )
    .unwrap();

    let fitted = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(fitted.spec.columns[2].source_name(), "Betrag Rp.");
    // …and nothing about it is flagged for review, because the plan itself
    // changes no value. The unit problem is not visible to the planner, which
    // is exactly why the alias is a human's to declare.
    assert!(fitted.review.is_none(), "{:?}", fitted.review);
}

/// The numeric twin of `a_genuinely_ambiguous_date_is_refused_…`, and it was
/// missing — which is how the planner came to accept a German column of
/// thousands as a column of units, silently, a thousandfold wrong.
///
/// `numfmt::infer` reports `ambiguous` when nothing in the column settles
/// which character is the decimal point. That verdict is the statement that
/// the answer is unknown, and it has to be honoured rather than stepped
/// around: `1.234` is either one-and-a-bit or one thousand two hundred and
/// thirty-four, and no proof in the file distinguishes them.
#[test]
fn an_ambiguous_decimal_separator_is_refused_until_the_convention_is_declared() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("de.csv");
    // German thousands: 1234, 2750, 12500, 9100. Read the Anglo way they are
    // a thousand times smaller, and every value still parses.
    std::fs::write(&p, "region;betrag\nNord;1.234\nSued;2.750\nOst;12.500\nWest;9.100\n")
        .unwrap();

    let undeclared = Target::parse(
        "CREATE TABLE t (region TEXT NOT NULL, betrag DECIMAL(14,2) NOT NULL) \
         WITH (files = 'de.csv')",
    )
    .unwrap();
    match fit(&p, &undeclared, Limits::default()) {
        Err(FitError::Gaps(g)) => {
            assert_eq!(g.len(), 1, "{g:?}");
            match &g[0] {
                Gap::AmbiguousSeparator { column, separator, .. } => {
                    assert_eq!(column, "betrag");
                    assert_eq!(*separator, '.');
                }
                other => panic!("expected AmbiguousSeparator, got {other:?}"),
            }
            assert!(g[0].message().contains("decimal_separator"), "{}", g[0].message());
        }
        Ok(f) => panic!(
            "an ambiguous separator was silently resolved: {:?}",
            f.spec.columns[1].parse
        ),
        Err(e) => panic!("{e}"),
    }

    // Declared, it fits — and reads the German values, not the Anglo ones.
    let declared = Target::parse(
        "CREATE TABLE t (region TEXT NOT NULL, betrag DECIMAL(14,2) NOT NULL) \
         WITH (files = 'de.csv', decimal_separator = ',')",
    )
    .unwrap();
    let f = fit(&p, &declared, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    let batch = tdy::provider::spec_to_batch(&f.spec, &p).unwrap();
    let col = batch
        .column(1)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap();
    let total: i128 = (0..col.len()).map(|i| col.value(i)).sum();
    // 1234 + 2750 + 12500 + 9100 = 25584, at scale 2.
    assert_eq!(total, 2_558_400, "the German reading was not used");
}

/// An unambiguous separator still works without any declaration — the
/// refusal above must not become "tdy cannot read decimals".
#[test]
fn an_unambiguous_separator_needs_no_declaration() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("en.csv");
    // Two fractional digits and a value whose integer part is four digits:
    // nothing here reads as thousands grouping.
    std::fs::write(&p, "region;betrag\nNord;1234.50\nSued;2750.25\n").unwrap();
    let t = Target::parse(
        "CREATE TABLE t (region TEXT NOT NULL, betrag DECIMAL(14,2) NOT NULL) \
         WITH (files = 'en.csv')",
    )
    .unwrap();
    let f = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    let batch = tdy::provider::spec_to_batch(&f.spec, &p).unwrap();
    let col = batch
        .column(1)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap();
    let total: i128 = (0..col.len()).map(|i| col.value(i)).sum();
    assert_eq!(total, 398_475);
}

/// A text column must not carry the missing-value vocabulary. "NA" is
/// Namibia, "NONE" is an answer, and nulling a real string is data loss no
/// later step can undo — the same reason `sniff` refuses to do it.
#[test]
fn a_fitted_text_column_keeps_values_that_look_like_null_tokens() {
    let dir = tempfile::TempDir::new().unwrap();
    let p = dir.path().join("c.csv");
    std::fs::write(&p, "country,code\nNamibia,NA\nNone of the above,NONE\nNorway,NO\n")
        .unwrap();
    let t = Target::parse(
        "CREATE TABLE c (country TEXT NOT NULL, code TEXT NOT NULL) WITH (files = 'c.csv')",
    )
    .unwrap();
    let f = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        f.spec.columns.iter().all(|c| c.parse.na_values.is_empty()),
        "a text column was given null tokens: {:?}",
        f.spec.columns
    );
    let batch = tdy::provider::spec_to_batch(&f.spec, &p).unwrap();
    let codes = batch
        .column(1)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::StringArray>()
        .unwrap();
    assert_eq!(
        (0..codes.len()).map(|i| codes.value(i)).collect::<Vec<_>>(),
        vec!["NA", "NONE", "NO"]
    );
}

// ---------------------------------------------------------------------------
// Regressions from the review of the planner. Each is a mechanism that was
// wrong in a way no fixture happened to exercise.
// ---------------------------------------------------------------------------

/// Two declared columns cannot both take the same column of the file.
///
/// tdy has no computed columns, so the two would hold byte-identical values —
/// a target that asks for `net` and `gross` and silently gets one number twice
/// is a typo in the target, and reporting it beats obeying it.
#[test]
fn two_declared_columns_may_not_bind_the_same_source_column() {
    let t = Target::parse(
        "CREATE TABLE twice (
           betrag  DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag'),
           amount  DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
         ) WITH (files = '*.csv', date_order = 'dmy')",
    )
    .unwrap();
    let err = fit(&corpus().join("2025-01.csv"), &t, Limits::default())
        .expect_err("both columns bind `Betrag`; that must not be a plan");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps") };
    let collides = gaps
        .iter()
        .find(|g| matches!(g, Gap::Collides { .. }))
        .expect("the collision must be reported as such");
    let m = collides.message();
    assert!(m.contains("Betrag"), "{m}");
    assert!(m.contains("twice") || m.contains("both bind"), "{m}");
}

/// `verify = 'full'` — the default — proves the declared type on **every**
/// row, not on the prefix `dry_run` reads.
///
/// `late_surprise_id_turns_alphanumeric.csv` is the reduction of a real Divvy
/// export: `station_id` is digits for seven hundred rows and then
/// `TA1309000067`. A planner that types from the head lands a plan that dies
/// mid-query on a file it declared fittable.
#[test]
fn a_type_that_breaks_past_the_sample_is_a_gap_not_a_plan() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("late_surprise_id_turns_alphanumeric.csv");
    let t = Target::parse(
        "CREATE TABLE trips (
           station_id BIGINT NOT NULL
         ) WITH (files = '*.csv')",
    )
    .unwrap();
    let err = fit(&file, &t, Limits::default()).expect_err("row 701 is not a number");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps, got a plan") };
    let m = gaps[0].message();
    assert!(m.contains("TA1309000067"), "the offending value must be named:\n{m}");
    assert!(m.contains("701"), "the row must be named:\n{m}");
}

/// …and `verify = 'head'` is the documented way to opt out of paying for it.
/// It must actually change what happens, or the option is decoration.
#[test]
fn verify_head_does_not_read_the_whole_file() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("late_surprise_id_turns_alphanumeric.csv");
    let sql = "CREATE TABLE trips (station_id BIGINT NOT NULL) WITH (files = '*.csv', verify = ";
    let full = Target::parse(&format!("{sql}'full')")).unwrap();
    let head = Target::parse(&format!("{sql}'head')")).unwrap();
    assert_eq!(full.verify, tdy::target::Verify::Full);
    assert_eq!(head.verify, tdy::target::Verify::Head);
    // The point of the pair: the same file, the same target, two answers —
    // and the expensive one is the default.
    assert!(fit(&file, &full, Limits::default()).is_err());
}

/// A fitted spec that drops rows must say so. The sniffer's auto-drop of a
/// byte-identical repeated header travels into the plan, and a plan that
/// removes rows silently is the failure this project is built against.
#[test]
fn a_fitted_spec_that_drops_rows_carries_the_note_that_says_so() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("late_surprise_repeated_header.csv");
    let t = Target::parse(
        "CREATE TABLE inv (
           invoice BIGINT NOT NULL,
           amount  BIGINT NOT NULL
         ) WITH (files = '*.csv')",
    )
    .unwrap();
    let fitted = fit(&file, &t, Limits::default()).expect("the repeat is provably not data");
    assert!(
        fitted.spec.notes.iter().any(|n| n.starts_with(tdy::sniff::DROPPED_NOTE)),
        "the drop was not reported:\n{:#?}",
        fitted.spec.notes
    );
    // And it really dropped exactly the one row.
    let b = tdy::provider::spec_to_batch(&fitted.spec, &file).unwrap();
    assert_eq!(b.num_rows(), 1000);
}

/// The mapping notes are machinery; the rounding note is a message. The CLI
/// filter used to hide anything starting with a backtick, which hid the one
/// note in the planner that says a value was changed.
#[test]
fn the_rounding_note_is_not_mistaken_for_a_binding_note() {
    assert!(tdy::fit::is_binding_note(&tdy::fit::binding_note("amount", "Betrag")));
    assert!(!tdy::fit::is_binding_note(
        "`amount`: some values carry more than 2 fractional digits and are rounded \
         half away from zero"
    ));
}

// ---------------------------------------------------------------------------
// Declared-absent columns and constants.
// ---------------------------------------------------------------------------

/// `if_missing = 'null'` is the declared-absent case: November predates the
/// `Region` column, and the target says so *in the declaration*, where it is
/// versioned and reviewed. The planner is then executing a decision, not
/// making one — which is why this fit carries a note but no review reason.
#[test]
fn a_declared_absent_column_is_null_filled_and_needs_no_review() {
    let t = Target::parse(
        "CREATE TABLE sales (
           month      DATE          NOT NULL OPTIONS(matches = 'Datum'),
           region     TEXT          NULL     OPTIONS(matches = 'Region', if_missing = 'null'),
           amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
         ) WITH (files = '*.csv', date_order = 'dmy')",
    )
    .unwrap();
    let p = corpus().join("2025-11.csv");
    let fitted = fit(&p, &t, Limits::default()).expect("the declaration makes it fit");
    assert!(fitted.review.is_none(), "a declared fill is not a judgement: {:?}", fitted.review);
    assert!(conforms(&fitted.spec, &t).is_ok());
    assert!(
        fitted.spec.notes.iter().any(|n| n.contains("if_missing")),
        "the fill must be said out loud:\n{:#?}",
        fitted.spec.notes
    );

    let batch = tdy::provider::spec_to_batch(&fitted.spec, &p).unwrap();
    assert_eq!(batch.num_columns(), 3);
    let region = batch.column(1);
    assert_eq!(region.null_count(), batch.num_rows(), "every region must be null");
    assert!(batch.num_rows() > 0);
}

/// …and without the declaration the same file stays refused — the fill is
/// opt-in per column, never a planner courtesy. (The corpus target has no
/// `if_missing`, and `a_file_missing_a_declared_column_is_refused_not_null_filled`
/// pins that half.)
///
/// A hand-written constant *value* is a different thing entirely: data the
/// file does not contain, asserted by a human, and gated exactly like
/// `decimal_shift`.
#[test]
fn a_constant_value_is_a_review_reason_a_null_fill_is_not() {
    use tdy::spec::Transform;
    let p = corpus().join("2025-11.csv");
    let t = Target::parse(
        "CREATE TABLE sales (
           month      DATE          NOT NULL OPTIONS(matches = 'Datum'),
           region     TEXT          NULL     OPTIONS(matches = 'Region', if_missing = 'null'),
           amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
         ) WITH (files = '*.csv', date_order = 'dmy')",
    )
    .unwrap();
    let fitted = fit(&p, &t, Limits::default()).unwrap();

    // The planner's own fill: no review.
    assert!(tdy::fit::review_reasons(&fitted.spec).is_empty());

    // The same spec with the fill turned into an asserted value: review.
    let mut spec = fitted.spec.clone();
    for tr in &mut spec.transforms {
        if let Transform::Constant { value, .. } = tr {
            *value = "Ticino".into();
        }
    }
    let reasons = tdy::fit::review_reasons(&spec);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("Ticino"), "{}", reasons[0]);
}

/// The declaration is refused where it contradicts itself or overreaches:
/// a NOT NULL column cannot be null-filled, and only 'null' is declarable —
/// a default *value* belongs in the sidecar, behind review.
#[test]
fn if_missing_is_refused_on_not_null_and_for_values() {
    let e = Target::parse(
        "CREATE TABLE t (region TEXT NOT NULL OPTIONS(if_missing = 'null'))
         WITH (files = '*.csv')",
    )
    .expect_err("NOT NULL + if_missing is a contradiction");
    assert!(format!("{e:#}").contains("NOT NULL"), "{e:#}");

    let e = Target::parse(
        "CREATE TABLE t (region TEXT OPTIONS(if_missing = 'Ticino'))
         WITH (files = '*.csv')",
    )
    .expect_err("a default value is not declarable");
    assert!(format!("{e:#}").contains("review"), "{e:#}");
}

// ---------------------------------------------------------------------------
// Frame elimination: JSON documents with several record arrays.
// ---------------------------------------------------------------------------

fn frames_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

const JSON_TARGET: &str = "CREATE TABLE orders (
    day    DATE          NOT NULL,
    region TEXT          NOT NULL,
    amount DECIMAL(14,2) NOT NULL
) WITH (files = '*.json')";

/// A document with four arrays, one of which produces the declared table.
/// The sniffer alone can only rank them and say it is unsure; the declaration
/// turns the ranking into a search whose answer is *proved*: every other
/// candidate was tried and failed.
#[test]
fn a_json_frame_is_proved_by_elimination_when_only_one_array_fits() {
    let t = Target::parse(JSON_TARGET).unwrap();
    let p = frames_fixture("json_frames_one_fits.json");
    let fitted = fit(&p, &t, Limits::default()).expect("only /orders fits");
    assert!(
        matches!(
            &fitted.spec.extraction,
            tdy::spec::Extraction::Json { pointer: Some(ptr), .. } if ptr == "/orders"
        ),
        "{:?}",
        fitted.spec.extraction
    );
    assert!(
        fitted.spec.notes.iter().any(|n| n.contains("elimination")),
        "the proof must be stated:\n{:#?}",
        fitted.spec.notes
    );
    // Elimination is a proof, not a judgement: nothing to review.
    assert!(fitted.review.is_none(), "{:?}", fitted.review);

    // And the right numbers come out.
    let b = tdy::provider::spec_to_batch(&fitted.spec, &p).unwrap();
    assert_eq!(b.num_rows(), 4);
    let amounts = b
        .column(2)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap();
    let total: i128 = (0..amounts.len()).map(|i| amounts.value(i)).sum();
    assert_eq!(total, 66000, "sum(amount) must be 660.00");
}

/// Two arrays that BOTH produce the declared table are two complete,
/// well-typed, different answers — q1 sums to 600.00 and q2 to 1500.00 — and
/// ranking them would be a guess with a plausible wrong number at the end.
/// Refused, naming both, with the sidecar remedy.
#[test]
fn two_fitting_arrays_are_refused_not_ranked() {
    let t = Target::parse(JSON_TARGET).unwrap();
    let p = frames_fixture("json_frames_two_fit.json");
    let err = fit(&p, &t, Limits::default()).expect_err("q1 and q2 both fit");
    let msg = format!("{err}");
    assert!(matches!(err, FitError::AmbiguousFrame { .. }), "{msg}");
    assert!(msg.contains("/q1") && msg.contains("/q2"), "{msg}");
    assert!(msg.contains("pointer"), "the remedy must be named:\n{msg}");
}

// ---------------------------------------------------------------------------
// Frame elimination, second domain: workbooks with several sheets.
// ---------------------------------------------------------------------------

const SHEET_TARGET: &str = "CREATE TABLE monat (
    month  DATE          NOT NULL OPTIONS(matches = 'Datum'),
    region TEXT          NOT NULL OPTIONS(matches = 'Region'),
    amount DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
) WITH (files = '*.xlsx', date_order = 'dmy')";

/// A cover page, a data sheet behind a title row, and a legend. The cover
/// page is deliberately the *biggest* sheet, so the sniffer's ranking alone
/// would not settle it — elimination does: each sheet is framed on its own
/// (the title row above `Daten`'s header is a fact about that sheet), the
/// declaration is tried against each, and only one survives.
#[test]
fn an_excel_frame_is_proved_by_elimination_when_only_one_sheet_fits() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let p = frames_fixture("sheet_frames_one_fits.xlsx");
    let fitted = fit(&p, &t, Limits::default()).expect("only 'Daten' fits");
    assert!(
        matches!(
            &fitted.spec.extraction,
            tdy::spec::Extraction::Excel { sheet_name: Some(sh), .. } if sh == "Daten"
        ),
        "{:?}",
        fitted.spec.extraction
    );
    assert!(
        fitted.spec.notes.iter().any(|n| n.contains("elimination")),
        "{:#?}",
        fitted.spec.notes
    );
    assert!(fitted.review.is_none(), "elimination is a proof: {:?}", fitted.review);

    let b = tdy::provider::spec_to_batch(&fitted.spec, &p).unwrap();
    assert_eq!(b.num_rows(), 4);
    let amounts = b
        .column(2)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap();
    let total: i128 = (0..amounts.len()).map(|i| amounts.value(i)).sum();
    assert_eq!(total, 109000, "sum(amount) must be 1090.00");
}

/// Q1 and Q2 both produce the declared table with different totals (600.00
/// vs 1500.00): two complete, well-typed, different answers. Refused, both
/// sheets named, with the sidecar remedy.
#[test]
fn two_fitting_sheets_are_refused_not_ranked() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let p = frames_fixture("sheet_frames_two_fit.xlsx");
    let err = fit(&p, &t, Limits::default()).expect_err("Q1 and Q2 both fit");
    let msg = format!("{err}");
    assert!(matches!(err, FitError::AmbiguousFrame { .. }), "{msg}");
    assert!(msg.contains("Q1") && msg.contains("Q2"), "{msg}");
    assert!(msg.contains("sheet_name"), "the remedy must be named:\n{msg}");
}

/// A long-format member meeting a wide target is not a missing column, and
/// saying "add `matches`" sends someone looking for something that is not
/// there. `q1` is not absent from the file — it is one of the *values* in its
/// `quarter` column, and the file needs reshaping that tdy has no operator for.
///
/// Catalogue D2. Rather than build a pivot for a shape that occurs in 2 of
/// 1,332 corpus files, the refusal names the situation and both real options.
#[test]
fn a_long_format_member_is_told_it_is_long_format() {
    let dir = tempfile::TempDir::new().unwrap();
    let f = dir.path().join("2025-long.csv");
    std::fs::write(&f, "region,quarter,umsatz\nOst,Q1,100\nOst,Q2,120\nWest,Q1,90\n").unwrap();
    let t = dir.path().join("sales.tdy.sql");
    std::fs::write(
        &t,
        "CREATE TABLE sales (\n  region TEXT NOT NULL,\n  q1 BIGINT,\n  q2 BIGINT\n)\n\
         WITH (\n  files = '2025-*.csv'\n);\n",
    )
    .unwrap();

    let target = Target::load(&t).expect("the target parses");
    let err = fit(&f, &target, Limits::default()).expect_err("a wide target cannot take long data");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps, got {err:?}") };

    let text = gaps.iter().map(|g| g.message()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("in long form"), "the situation must be named: {text}");
    assert!(text.contains("\"quarter\""), "and which column holds the names: {text}");
    assert!(text.contains("no pivot"), "and why tdy cannot fix it: {text}");
    assert!(
        !text.contains("OPTIONS(matches"),
        "advising `matches` here sends the reader after a column that does not exist:\n{text}"
    );
}

/// A file and a target, for the long-form cases below.
fn fit_pair(csv: &str, ddl: &str) -> (tempfile::TempDir, PathBuf, Target) {
    let dir = tempfile::TempDir::new().unwrap();
    let f = dir.path().join("2025-x.csv");
    std::fs::write(&f, csv).unwrap();
    let t = dir.path().join("t.tdy.sql");
    std::fs::write(&t, ddl).unwrap();
    let target = Target::load(&t).expect("the target parses");
    (dir, f, target)
}

fn gap_text(f: &Path, target: &Target) -> String {
    let err = fit(f, target, Limits::default()).expect_err("must not fit");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps, got {err:?}") };
    gaps.iter().map(|g| g.message()).collect::<Vec<_>>().join("\n")
}

/// One declared name turning up once among some column's values is not a
/// long-format file: a `Kind` column holding `detail` and `total` is a
/// category, and a `total` target column is a merely-misspelled `Summe`.
/// Diagnosing long form here would withhold the one remedy that works.
#[test]
fn a_category_value_equal_to_a_column_name_is_not_long_form() {
    let (_d, f, t) = fit_pair(
        "Region,Kind,Summe\nOst,detail,10\nOst,total,10\nWest,detail,5\n",
        "CREATE TABLE s (region TEXT NOT NULL, total DECIMAL(14,2)) WITH (files = '2025-*.csv');",
    );
    let text = gap_text(&f, &t);
    assert!(text.contains("OPTIONS(matches"), "the matches remedy must stay: {text}");
    assert!(!text.contains("in long form"), "{text}");
}

/// Likewise a subtotal label interleaved in a key column.
#[test]
fn an_interleaved_subtotal_label_is_not_long_form() {
    let (_d, f, t) = fit_pair(
        "Region,Betrag\nOst,10\nTotal,10\nWest,5\nTotal,5\n",
        "CREATE TABLE s (region TEXT NOT NULL, total DECIMAL(14,2)) WITH (files = '2025-*.csv');",
    );
    let text = gap_text(&f, &t);
    assert!(text.contains("OPTIONS(matches"), "the matches remedy must stay: {text}");
    assert!(!text.contains("in long form"), "{text}");
}

/// The signature is several declared columns appearing as values of the
/// *same* column — and it has to hold over the whole probe, or a long file
/// sorted by its key (all the Q1 rows, then all the Q2 rows) is diagnosed
/// long-form for `q1` and sent hunting for a `matches` spelling for `q2`.
#[test]
fn a_long_file_sorted_by_key_is_long_form_for_every_declared_column() {
    let mut csv = String::from("region,quarter,umsatz\n");
    for i in 0..600 {
        csv.push_str(&format!("R{i},Q1,{i}\n"));
    }
    for i in 0..600 {
        csv.push_str(&format!("R{i},Q2,{i}\n"));
    }
    let (_d, f, t) = fit_pair(
        &csv,
        "CREATE TABLE s (region TEXT NOT NULL, q1 BIGINT, q2 BIGINT) WITH (files = '2025-*.csv');",
    );
    let err = fit(&f, &t, Limits::default()).expect_err("a wide target cannot take long data");
    let FitError::Gaps(gaps) = err else { panic!("expected gaps, got {err:?}") };
    for g in &gaps {
        let m = g.message();
        assert!(m.contains("in long form"), "{}: {m}", g.column());
        assert!(!m.contains("OPTIONS(matches"), "{}: {m}", g.column());
    }
    assert_eq!(gaps.len(), 2);
}

/// "Is this value the column's name?" is the planner's own question, so it
/// uses the planner's own notion of sameness: `norm` under the default mode
/// (Unicode case, `_` and space folded), and byte equality under `exact`.
#[test]
fn long_form_is_judged_by_the_targets_own_name_matching() {
    let csv = "kanton,monat,betrag\nZH,MÄRZ,1\nZH,APRIL,2\nBE,MÄRZ,3\nBE,APRIL,4\n";
    let (_d, f, t) = fit_pair(
        csv,
        "CREATE TABLE s (kanton TEXT NOT NULL, märz BIGINT, april BIGINT) WITH (files = '2025-*.csv');",
    );
    let text = gap_text(&f, &t);
    assert!(text.contains("in long form"), "normalized mode folds case: {text}");

    let (_d2, f2, t2) = fit_pair(
        csv,
        "CREATE TABLE s (kanton TEXT NOT NULL, märz BIGINT, april BIGINT) \
         WITH (files = '2025-*.csv', match = 'exact');",
    );
    let text = gap_text(&f2, &t2);
    assert!(!text.contains("in long form"), "exact mode does not: {text}");
    assert!(text.contains("OPTIONS(matches"), "{text}");
}

/// A declared alias counts as the column's name here too.
#[test]
fn long_form_sees_declared_matches_spellings() {
    let (_d, f, t) = fit_pair(
        "region,quarter,umsatz\nOst,Q1,100\nOst,Q2,120\n",
        "CREATE TABLE s (region TEXT NOT NULL, first BIGINT OPTIONS(matches = 'Q1'), \
         second BIGINT OPTIONS(matches = 'Q2')) WITH (files = '2025-*.csv');",
    );
    let text = gap_text(&f, &t);
    assert!(text.contains("in long form"), "{text}");
}

/// The diagnosis has to travel with the problem, not only in its prose: a
/// `--json`/MCP consumer reads `kind`, and the remedy menu is built from it.
#[test]
fn a_long_form_gap_is_reported_structurally() {
    let (_d, f, t) = fit_pair(
        "region,quarter,umsatz\nOst,Q1,100\nOst,Q2,120\n",
        "CREATE TABLE s (region TEXT NOT NULL, q1 BIGINT, q2 BIGINT) WITH (files = '2025-*.csv');",
    );
    let err = fit(&f, &t, Limits::default()).expect_err("must not fit");
    let problems = tdy::report::problems_json(&err);
    let problems = problems.as_array().expect("an array of problems");
    assert_eq!(problems.len(), 2, "{problems:?}");
    for p in problems {
        assert_eq!(p["kind"], "long_form", "{p}");
        assert_eq!(p["long_form"], "quarter", "{p}");
        assert!(p["header"].as_array().map(|h| !h.is_empty()).unwrap_or(false), "{p}");
    }
}

/// `propose` ranks the file's columns by whether their values can produce
/// the declared type — which `umsatz` can, for `q1`, and binding it would put
/// every quarter's amount into one column. A long-form column has no
/// candidate to offer.
#[test]
fn propose_offers_nothing_for_a_long_form_column() {
    let (_d, f, t) = fit_pair(
        "region,quarter,umsatz\nOst,Q1,100\nOst,Q2,120\n",
        "CREATE TABLE s (region TEXT NOT NULL, q1 BIGINT, q2 BIGINT) WITH (files = '2025-*.csv');",
    );
    let proposals = tdy::fit::propose(&f, &t, Limits::default()).unwrap();
    assert!(proposals.is_empty(), "{proposals:?}");
}

/// ...and an ordinary missing column still gets the ordinary advice, which is
/// the one that works when the column really could be supplied.
#[test]
fn an_ordinary_missing_column_still_suggests_matches() {
    let gaps = gap_of("2025-11.csv");
    let text = gaps.iter().map(|g| g.message()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("OPTIONS(matches"), "{text}");
    assert!(!text.contains("in long form"), "nothing here holds a column name as a value:\n{text}");
}

// ---------------------------------------------------------------------------
// Sheet discovery and single-sheet fitting, for the pile-level fit (Task 6).
// ---------------------------------------------------------------------------

/// Discovery says which sheets pass the gates, in the workbook's order, and
/// names the ones that do not — without choosing.
#[test]
fn discovery_names_the_fitting_sheets_and_the_rejected_ones() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let two = discover_sheets(&frames_fixture("sheet_frames_two_fit.xlsx"), &t, Limits::default())
        .unwrap()
        .expect("a two-sheet workbook is discoverable");
    assert_eq!(two.total, 2);
    assert_eq!(two.fitting, vec!["Q1".to_string(), "Q2".to_string()]);
    assert!(two.rejected.is_empty());

    let one = discover_sheets(&frames_fixture("sheet_frames_one_fits.xlsx"), &t, Limits::default())
        .unwrap()
        .expect("three sheets");
    assert_eq!(one.total, 3);
    assert_eq!(one.fitting, vec!["Daten".to_string()]);
    assert_eq!(one.rejected, vec!["Hinweise".to_string(), "Legende".to_string()]);

    let csv = corpus().join("2025-01.csv");
    assert!(discover_sheets(&csv, &target(), Limits::default()).unwrap().is_none(), "not a workbook");

    // One sheet is not a choice, so there is nothing to discover — and the
    // answer costs one read of the workbook's shape, not a sniff of it.
    let one_sheet = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/umsatz.xlsx");
    assert!(
        discover_sheets(&one_sheet, &target(), Limits::default()).unwrap().is_none(),
        "a single-sheet workbook is a plain member"
    );
}

/// One named sheet, fully fitted: its own frame, its own sum.
#[test]
fn a_named_sheet_is_fitted_on_its_own() {
    let t = Target::parse(SHEET_TARGET).unwrap();
    let p = frames_fixture("sheet_frames_two_fit.xlsx");
    let q2 = fit_sheet(&p, "Q2", &t, Limits::default()).expect("Q2 fits");
    assert!(matches!(&q2.spec.extraction, tdy::spec::Extraction::Excel { sheet_name: Some(s), .. } if s == "Q2"));
    let batches = tdy::engine::execute_batches(&q2.spec, &p, Limits::default()).unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 3);
    let total: i128 = batches
        .iter()
        .flat_map(|b| {
            let a = b.column_by_name("amount").unwrap();
            let a = a.as_any().downcast_ref::<datafusion::arrow::array::Decimal128Array>().unwrap();
            (0..a.len()).map(|i| a.value(i)).collect::<Vec<_>>()
        })
        .sum();
    assert_eq!(total, 150000, "sum(amount) of Q2 is 1500.00");
    assert!(fit_sheet(&p, "Q9", &t, Limits::default()).is_err(), "a sheet that does not exist");
}

// ---------------------------------------------------------------------------
// A declaration authorises a reading: `year_pivot` and `epoch` on a target
// column. Without one, neither reading is ever tried.
// ---------------------------------------------------------------------------

fn dates_of(spec: &tdy::spec::ParseSpec, f: &Path, i: usize) -> Vec<String> {
    let b = tdy::provider::spec_to_batch(spec, f).unwrap();
    let a = b.column(i).as_any().downcast_ref::<datafusion::arrow::array::Date32Array>().unwrap();
    (0..a.len()).map(|r| a.value_as_date(r).unwrap().to_string()).collect()
}

/// `01.02.29` is 2029 and `01.02.45` is 1945 under pivot 30, and the century
/// came from the reviewed declaration, so the plan carries a note and no
/// review. Without the declaration the file is a gap that names it.
#[test]
fn a_declared_year_pivot_reads_two_digit_years_with_a_note_and_no_review() {
    let csv = "Datum;Betrag\n01.02.29;10\n01.02.45;20\n13.03.30;5\n";
    let (_d, f, t) = fit_pair(
        csv,
        "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum', year_pivot = '30'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv', date_order = 'dmy')",
    );
    let fitted = fit(&f, &t, Limits::default()).unwrap();
    assert_eq!(fitted.review, None, "{:?}", fitted.notes);
    assert!(
        fitted.notes.iter().any(|n| n
            == "`datum`: two-digit years are read as 1930–2029 (year_pivot 30), as the target declares"),
        "{:?}",
        fitted.notes
    );
    assert!(tdy::fit::review_reasons_for(&fitted.spec, &t).is_empty());
    assert_eq!(dates_of(&fitted.spec, &f, 0), ["2029-02-01", "1945-02-01", "1930-03-13"]);

    let (_d, f, t) = fit_pair(
        csv,
        "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv', date_order = 'dmy')",
    );
    let text = gap_text(&f, &t);
    assert!(
        text.contains("two-digit years, whose century no value states; declare the window by adding `year_pivot = '…'` to the OPTIONS of `datum`"),
        "{text}"
    );
}

/// The declaration authorises the planner's reading, not any hand-written
/// one: a manual spec reading `%y` under a target that declares no pivot (or
/// another pivot) still waits on a person.
#[test]
fn a_manual_two_digit_year_without_the_declaration_keeps_its_review() {
    let csv = "Datum;Betrag\n01.02.29;10\n01.02.45;20\n";
    let declared = "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum', year_pivot = '30'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv', date_order = 'dmy')";
    let (_d, f, t) = fit_pair(csv, declared);
    let spec = fit(&f, &t, Limits::default()).unwrap().spec;
    for other in [
        declared.replace(", year_pivot = '30'", ""),
        declared.replace("year_pivot = '30'", "year_pivot = '50'"),
    ] {
        let t2 = Target::parse(&other).unwrap();
        let r = tdy::fit::review_reasons_for(&spec, &t2);
        assert_eq!(r.len(), 1, "{other}: {r:?}");
        assert!(r[0].contains("reads two-digit years as 1930–2029"), "{}", r[0]);
    }
}

/// A column declared `epoch = 'excel_days'` binds integers through that unit
/// — the only reading tried — with a note and no review; serial 59 is refused
/// naming its row.
#[test]
fn a_declared_excel_epoch_reads_serials_as_dates() {
    let ddl = "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum', epoch = 'excel_days'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv')";
    let (_d, f, t) = fit_pair("Datum;Betrag\n45000;10\n45001;20\n45351;5\n", ddl);
    let fitted = fit(&f, &t, Limits::default()).unwrap();
    assert_eq!(fitted.review, None, "{:?}", fitted.notes);
    assert!(
        fitted.notes.iter().any(|n| n == "`datum`: read as spreadsheet serial days (epoch excel_days), as the target declares"),
        "{:?}",
        fitted.notes
    );
    assert_eq!(dates_of(&fitted.spec, &f, 0), ["2023-03-15", "2023-03-16", "2024-02-29"]);
    assert!(conforms(&fitted.spec, &t).is_ok());

    // The only reading: an ISO date is not read past the declaration.
    let (_d, f, t) = fit_pair("Datum;Betrag\n2023-03-15;10\n", ddl);
    assert!(gap_text(&f, &t).contains("is not a spreadsheet serial"));

    let (_d, f, t) = fit_pair("Datum;Betrag\n45000;10\n59;20\n", ddl);
    let text = gap_text(&f, &t);
    assert!(text.contains("row 2: cannot parse \"59\""), "{text}");
    assert!(text.contains("below 61"), "{text}");

    // Undeclared, integers are no date at all.
    let (_d, f, t) = fit_pair(
        "Datum;Betrag\n45000;10\n",
        "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv')",
    );
    let text = gap_text(&f, &t);
    assert!(text.contains("`datum` (DATE): reads \"Datum\""), "{text}");
}

/// End to end, twice: the second `tdy fit` reuses the sidecar the first
/// wrote, and the declaration still authorises its `%y` — no member waits on a
/// person, and the dataset answers with the declared century.
#[test]
fn a_declared_year_pivot_survives_a_refit_and_queries_without_accept() {
    let (d, _f, _t) = fit_pair(
        "Datum;Betrag\n01.02.29;10\n01.02.45;20\n",
        "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum', year_pivot = '30'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv', date_order = 'dmy')",
    );
    let t = d.path().join("t.tdy.sql");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
    };
    for _ in 0..2 {
        let out = run(&["fit", t.to_str().unwrap()]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(!text.contains("REVIEW"), "{text}");
    }
    let sql = format!("SELECT min(datum) AS lo, max(datum) AS hi FROM dataset('{}')", t.display());
    let out = run(&["query", &sql]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("1945-02-01") && text.contains("2029-02-01"), "{text}");
}

/// Under a declared pivot every day/month/year order is tried for each
/// separator, so `date_order` decides between them instead of the one order
/// a short list happened to hold: `15-03-24` under `dmy` is 2024-03-15, not
/// 2015-03-24, and `24/03/15` under `ymd` is 2024-03-15, not 2015-03-24.
#[test]
fn a_declared_year_pivot_tries_every_order_and_date_order_decides() {
    let ddl = |order: &str| {
        format!(
            "CREATE TABLE s (datum DATE NOT NULL OPTIONS(year_pivot = '30'), betrag BIGINT NOT NULL) \
             WITH (files = '*.csv'{order})"
        )
    };
    let dmy = "datum;betrag\n15-03-24;10\n28-02-25;20\n";
    let (_d, f, t) = fit_pair(dmy, &ddl(", date_order = 'dmy'"));
    let fitted = fit(&f, &t, Limits::default()).unwrap();
    assert_eq!(fitted.review, None);
    assert_eq!(dates_of(&fitted.spec, &f, 0), ["2024-03-15", "2025-02-28"]);

    let ymd = "datum;betrag\n24/03/15;10\n25/11/28;20\n";
    let (_d, f, t) = fit_pair(ymd, &ddl(", date_order = 'ymd'"));
    let fitted = fit(&f, &t, Limits::default()).unwrap();
    assert_eq!(dates_of(&fitted.spec, &f, 0), ["2024-03-15", "2025-11-28"]);

    // Undeclared order: the readings disagree, and nothing settles them.
    for csv in [dmy, ymd] {
        let (_d, f, t) = fit_pair(csv, &ddl(""));
        let text = gap_text(&f, &t);
        assert!(text.contains("parses under more than one format, and they disagree"), "{text}");
    }
}

fn tdy_cli(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// A pile of one serial-date file under a target, fitted; returns the dir,
/// the target path and the member's sidecar path.
fn epoch_pile(option: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (d, f, _t) = fit_pair(
        "Datum;Betrag\n45000;10\n45001;20\n",
        &format!(
            "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum'{option}), \
             betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv')"
        ),
    );
    let t = d.path().join("t.tdy.sql");
    (d, t, tdy::sidecar::sidecar_path(&f))
}

/// A target's declared `epoch` is enforced on a member's spec, not advised:
/// a sidecar reading the column with another unit contradicts the declaration
/// — at `tdy fit` for a hand-written one, and at every `dataset()` query for
/// one edited after its fit — naming both units. It used to serve 1970-01-01.
#[test]
fn a_declared_epoch_is_enforced_on_a_member_s_sidecar() {
    let (_d, t, sc) = epoch_pile(", epoch = 'excel_days'");
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(ok, "{text}");
    let planned = std::fs::read_to_string(&sc).unwrap();
    assert!(planned.contains("epoch = \"excel_days\""), "{planned}");
    let edited = planned.replace("epoch = \"excel_days\"", "epoch = \"seconds\"");
    std::fs::write(&sc, &edited).unwrap();

    // Edited after the fit: every query re-proves the member, and refuses.
    let sql = format!("SELECT min(datum) FROM dataset('{}')", t.display());
    let (ok, text) = tdy_cli(&["query", &sql]);
    assert!(!ok, "an edited epoch was served: {text}");
    assert!(
        text.contains("`datum`: the target declares epoch = 'excel_days', the spec reads it with epoch = 'seconds'"),
        "{text}"
    );

    // Hand-written: a contradiction the person has to settle.
    std::fs::write(&sc, edited.replace("method = \"heuristic\"", "method = \"manual\"")).unwrap();
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(!ok, "{text}");
    assert!(text.contains("CONTRADICTS"), "{text}");
    assert!(text.contains("the spec reads it with epoch = 'seconds'"), "{text}");
}

/// Any epoch reading in a sidecar is a judgement — that `45000` is a date at
/// all — so it waits on a person unless the target column declares the same
/// unit. One unix unit and the spreadsheet one, each with and without the
/// declaration.
#[test]
fn an_epoch_reading_waits_on_a_person_unless_the_target_declares_it() {
    use tdy::spec::{DType, EpochUnit};
    for (unit, name) in [(EpochUnit::Seconds, "seconds"), (EpochUnit::ExcelDays, "excel_days")] {
        let undeclared = Target::parse(
            "CREATE TABLE s (datum DATE NOT NULL, betrag BIGINT NOT NULL) WITH (files = '*.csv')",
        )
        .unwrap();
        let declared = Target::parse(&format!(
            "CREATE TABLE s (datum DATE NOT NULL OPTIONS(epoch = '{name}'), betrag BIGINT NOT NULL) \
             WITH (files = '*.csv')"
        ))
        .unwrap();
        let other = Target::parse(
            "CREATE TABLE s (datum DATE NOT NULL OPTIONS(epoch = 'milliseconds'), betrag BIGINT NOT NULL) \
             WITH (files = '*.csv')",
        )
        .unwrap();
        let mut spec = tdy::spec::ParseSpec {
            extraction: tdy::spec::Extraction::Delimited {
                delimiter: ';',
                quote: Some('"'),
                escape: None,
                encoding: None,
                comment: None,
                ragged: tdy::spec::RaggedPolicy::PadNulls,
                region: None,
            },
            transforms: vec![],
            columns: vec![],
            confidence: None,
            notes: vec![],
        };
        let mut c = tdy::spec::ColumnSpec {
            name: "datum".into(),
            source: None,
            dtype: DType::Date { format: "%s".into() },
            nullable: false,
            parse: Default::default(),
            pointer: None,
        };
        c.parse.epoch = Some(unit);
        spec.columns.push(c);
        let want = format!("`datum` reads integers as time (epoch = {name}), which no value in the file states");
        assert_eq!(tdy::fit::review_reasons(&spec), vec![want.clone()]);
        assert_eq!(tdy::fit::review_reasons_for(&spec, &undeclared), vec![want.clone()]);
        assert_eq!(tdy::fit::review_reasons_for(&spec, &other), vec![want], "{name}");
        assert!(tdy::fit::review_reasons_for(&spec, &declared).is_empty(), "{name}");
    }
}

/// End to end: a hand-written serial reading under a target that declares no
/// epoch is not served until a person accepts it.
#[test]
fn a_hand_written_epoch_under_no_declaration_waits_for_accept() {
    let (d, t, sc) = epoch_pile(", epoch = 'excel_days'");
    assert!(tdy_cli(&["fit", t.to_str().unwrap()]).0);
    let manual = std::fs::read_to_string(&sc).unwrap().replace("method = \"heuristic\"", "method = \"manual\"");
    std::fs::write(&sc, manual).unwrap();
    std::fs::write(
        &t,
        "CREATE TABLE s (datum DATE NOT NULL OPTIONS(matches = 'Datum'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv')",
    )
    .unwrap();
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(text.contains("REVIEW: `datum` reads integers as time (epoch = excel_days)"), "{text}");
    let sql = format!("SELECT min(datum) AS lo FROM dataset('{}')", t.display());
    assert!(!tdy_cli(&["query", &sql]).0, "served before acceptance");
    let member = d.path().join("2025-x.csv");
    assert!(tdy_cli(&["fit", t.to_str().unwrap(), "--accept", "2025-x.csv"]).0, "{}", member.display());
    let (ok, text) = tdy_cli(&["query", &sql]);
    assert!(ok && text.contains("2023-03-15"), "{text}");
}

/// `format = "%s"` with no `epoch` is epoch seconds — chrono's own specifier —
/// so it is the same judgement as `epoch = "seconds"`: reviewed unless the
/// target declares `epoch = 'seconds'`, and conforming when it does. It used to
/// slip past the review (served 2023-11-14T22:13:20 unasked) and to contradict
/// a declaration it reads identically to.
#[test]
fn a_bare_percent_s_is_epoch_seconds_for_review_and_conformance() {
    let dir = tempfile::TempDir::new().unwrap();
    let f = dir.path().join("a.csv");
    std::fs::write(&f, "ts\n1700000000\n1700000060\n").unwrap();
    let t = dir.path().join("t.tdy.sql");
    let declared = "CREATE TABLE s (ts TIMESTAMP NOT NULL OPTIONS(epoch = 'seconds')) WITH (files = '*.csv')";
    std::fs::write(&t, declared).unwrap();
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(ok, "{text}");
    let sc = tdy::sidecar::sidecar_path(&f);
    let planned = std::fs::read_to_string(&sc).unwrap();
    assert!(planned.contains("format = \"%s\"") && planned.contains("epoch = \"seconds\""), "{planned}");
    let bare: String = planned
        .lines()
        .filter(|l| l.trim() != "epoch = \"seconds\"")
        .map(|l| format!("{l}\n"))
        .collect::<String>()
        .replace("method = \"heuristic\"", "method = \"manual\"");
    std::fs::write(&sc, bare).unwrap();
    let sql = format!("SELECT min(ts) AS lo FROM dataset('{}')", t.display());

    // The mirror: declared seconds, `%s` alone — conforms, no review, served.
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(ok && !text.contains("REVIEW") && !text.contains("CONTRADICTS"), "{text}");
    let (ok, text) = tdy_cli(&["query", &sql]);
    assert!(ok && text.contains("2023-11-14T22:13:20"), "{text}");

    // Undeclared: the same sidecar waits on a person.
    std::fs::write(&t, "CREATE TABLE s (ts TIMESTAMP NOT NULL) WITH (files = '*.csv')").unwrap();
    let (ok, text) = tdy_cli(&["fit", t.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(text.contains("REVIEW: `ts` reads integers as time (epoch = seconds)"), "{text}");
    assert!(!tdy_cli(&["query", &sql]).0, "served before acceptance");

    // messy() is no pile: unaffected.
    let (ok, text) = tdy_cli(&["query", &format!("SELECT min(ts) AS lo FROM messy('{}')", f.display())]);
    assert!(ok && text.contains("2023-11-14T22:13:20"), "{text}");

    assert!(tdy_cli(&["fit", t.to_str().unwrap(), "--accept", "a.csv"]).0);
    let (ok, text) = tdy_cli(&["query", &sql]);
    assert!(ok && text.contains("2023-11-14T22:13:20"), "{text}");
}

fn ts_strings(spec: &tdy::spec::ParseSpec, f: &Path, i: usize) -> Vec<String> {
    let b = tdy::provider::spec_to_batch(spec, f).unwrap();
    let a = b
        .column(i)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::TimestampMicrosecondArray>()
        .unwrap();
    (0..a.len()).map(|r| a.value_as_datetime(r).unwrap().to_string()).collect()
}

/// A target's declared decimal comma reaches a declared epoch column as it
/// reaches a numeric one: `45000,5` is noon.
#[test]
fn a_declared_excel_epoch_honours_the_declared_decimal_separator() {
    let (_d, f, t) = fit_pair(
        "Zeit;Betrag\n45000,5;10\n45001,25;20\n",
        "CREATE TABLE s (zeit TIMESTAMP NOT NULL OPTIONS(matches = 'Zeit', epoch = 'excel_days'), \
         betrag BIGINT NOT NULL OPTIONS(matches = 'Betrag')) WITH (files = '*.csv', decimal_separator = ',')",
    );
    let fitted = fit(&f, &t, Limits::default()).unwrap();
    assert_eq!(ts_strings(&fitted.spec, &f, 0), ["2023-03-15 12:00:00", "2023-03-16 06:00:00"]);
}

/// An ambiguity names the orders actually in conflict, not always `dmy`:
/// `13/02/14` is day-first and year-first at once under a declared pivot.
#[test]
fn an_ambiguous_date_names_the_orders_in_conflict() {
    for order in ["", ", date_order = 'mdy'"] {
        let (_d, f, t) = fit_pair(
            "datum;betrag\n13/02/14;10\n15/03/16;20\n",
            &format!(
                "CREATE TABLE s (datum DATE NOT NULL OPTIONS(year_pivot = '30'), betrag BIGINT NOT NULL) \
                 WITH (files = '*.csv'{order})"
            ),
        );
        let text = gap_text(&f, &t);
        assert!(
            text.contains("Declare which of these orders the exports use: WITH (date_order = 'dmy') or WITH (date_order = 'ymd')."),
            "{order}: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// A document is a record: one JSON object per file, nested fields reached
// through a declared `pointer`.
// ---------------------------------------------------------------------------

fn record_pile(docs: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for (name, body) in docs {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    dir
}

const ITEMS: &[(&str, &str)] = &[
    ("cap.json", r#"{"id":"cap","name":"1-up Cap","category":"Hats","games":{"nh":{"sellPrice":{"currency":"bells","value":80}}}}"#),
    ("cake.json", r#"{"id":"cake","name":"2017 Cake","category":"Food","games":{"nh":{"sellPrice":{"currency":"bells","value":250},"orderable":true}}}"#),
    ("lamp.json", r#"{"category":"Furniture","name":"Lamp","id":"lamp","games":{"nh":{"sellPrice":{"currency":"bells","value":1200}}}}"#),
];

fn ints_of(spec: &tdy::spec::ParseSpec, p: &Path, col: usize) -> Vec<Option<i64>> {
    let b = tdy::provider::spec_to_batch(spec, p).unwrap();
    let a = b.column(col).as_any().downcast_ref::<datafusion::arrow::array::Int64Array>().unwrap();
    (0..a.len()).map(|i| (!a.is_null(i)).then(|| a.value(i))).collect()
}

/// Each file is one object; the target names its keys, in any order the
/// files happen to write them. Nothing to eliminate (no array anywhere), so
/// no elimination note and nothing to review.
#[test]
fn a_pile_of_one_object_documents_fits_by_name() {
    let dir = record_pile(ITEMS);
    let t = Target::parse(
        "CREATE TABLE items (id TEXT NOT NULL, name TEXT NOT NULL, category TEXT NOT NULL) \
         WITH (files = '*.json')",
    )
    .unwrap();
    for (name, _) in ITEMS {
        let p = dir.path().join(name);
        let fitted = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{name}:\n{e}"));
        assert!(
            matches!(fitted.spec.extraction, tdy::spec::Extraction::Json { record: true, pointer: None, lines: false }),
            "{name}: {:?}",
            fitted.spec.extraction
        );
        assert!(fitted.review.is_none(), "{name}: {:?}", fitted.review);
        assert!(!fitted.notes.iter().any(|n| n.contains("elimination")), "{name}: {:?}", fitted.notes);
        let b = tdy::provider::spec_to_batch(&fitted.spec, &p).unwrap();
        assert_eq!(b.num_rows(), 1, "{name}");
    }
}

/// `matches` binds the top-level key, `pointer` reaches inside its value,
/// and two columns may open the same key at different places without
/// colliding.
#[test]
fn a_nested_leaf_binds_through_options_pointer() {
    let dir = record_pile(ITEMS);
    let t = Target::parse(
        "CREATE TABLE items (
            id         TEXT   NOT NULL,
            sell_price BIGINT NOT NULL OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value'),
            currency   TEXT   NOT NULL OPTIONS(matches = 'games', pointer = '/nh/sellPrice/currency'),
            orderable  BOOLEAN        OPTIONS(matches = 'games', pointer = '/nh/orderable')
        ) WITH (files = '*.json')",
    )
    .unwrap();
    let mut total = 0;
    for (name, _) in ITEMS {
        let p = dir.path().join(name);
        let fitted = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{name}:\n{e}"));
        let sell = &fitted.spec.columns[1];
        assert_eq!(sell.source.as_deref(), Some("games"), "{name}");
        assert_eq!(sell.pointer.as_deref(), Some("/nh/sellPrice/value"), "{name}");
        assert_eq!(fitted.spec.columns[2].pointer.as_deref(), Some("/nh/sellPrice/currency"));
        assert!(conforms(&fitted.spec, &t).is_ok());
        total += ints_of(&fitted.spec, &p, 1)[0].unwrap();
    }
    assert_eq!(total, 80 + 250 + 1200);
}

/// The executor's semantics, at plan time: a pointer that resolves to
/// nothing is a null, and a NOT NULL column refuses the member, naming the
/// column.
#[test]
fn a_not_null_column_whose_pointer_resolves_to_nothing_refuses_the_member() {
    let dir = record_pile(&[("old.json", r#"{"id":"old","games":{"nl":{"sellPrice":{"value":10}}}}"#)]);
    let t = Target::parse(
        "CREATE TABLE items (
            id         TEXT   NOT NULL,
            sell_price BIGINT NOT NULL OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value')
        ) WITH (files = '*.json')",
    )
    .unwrap();
    let e = fit(&dir.path().join("old.json"), &t, Limits::default()).expect_err("no /nh in this one");
    let m = format!("{e}");
    assert!(m.contains("sell_price") && m.contains("finds nothing") && m.contains("row 1"), "{m}");
    // The header leads with the pointer, not with a type failure.
    assert!(m.contains("`sell_price` (BIGINT): pointer \"/nh/sellPrice/value\" finds nothing"), "{m}");
    assert!(!m.contains("whose values cannot produce that type"), "{m}");

    // Nullable, the same document fits, with a null.
    let t = Target::parse(
        "CREATE TABLE items (
            id         TEXT   NOT NULL,
            sell_price BIGINT OPTIONS(matches = 'games', pointer = '/nh/sellPrice/value')
        ) WITH (files = '*.json')",
    )
    .unwrap();
    let p = dir.path().join("old.json");
    let fitted = fit(&p, &t, Limits::default()).unwrap();
    assert_eq!(ints_of(&fitted.spec, &p, 1), vec![None]);
}

/// A pointer onto an object or an array is an error, as in the sidecar: the
/// column would hold JSON text again.
#[test]
fn a_pointer_onto_a_container_is_a_gap() {
    let dir = record_pile(ITEMS);
    let t = Target::parse(
        "CREATE TABLE items (id TEXT, price TEXT OPTIONS(matches = 'games', pointer = '/nh/sellPrice')) \
         WITH (files = '*.json')",
    )
    .unwrap();
    let e = fit(&dir.path().join("cap.json"), &t, Limits::default()).expect_err("lands on an object");
    let m = format!("{e}");
    assert!(m.contains("price") && m.contains("an object"), "{m}");
}

/// `pointer` reads inside a JSON value; on a member read as anything else it
/// is refused at fit time with the sidecar's own rule.
#[test]
fn a_pointer_on_a_member_that_is_not_json_is_refused() {
    let dir = record_pile(&[("x.csv", "id,games\na,1\nb,2\n")]);
    let t = Target::parse(
        "CREATE TABLE items (id TEXT, n BIGINT OPTIONS(matches = 'games', pointer = '/nh')) \
         WITH (files = '*.csv')",
    )
    .unwrap();
    let e = fit(&dir.path().join("x.csv"), &t, Limits::default()).expect_err("csv has no JSON inside");
    let m = format!("{e}");
    assert!(m.contains("`pointer` reads inside a JSON value, and this file is read as"), "{m}");
}

/// A root object is a record AND holds an array of records, and both
/// produce the declared table: two complete, well-typed, different answers
/// (one row against two). Refused, naming both, with both settings that
/// settle it.
#[test]
fn a_record_and_an_array_that_both_fit_are_an_ambiguous_frame() {
    let dir = record_pile(&[(
        "both.json",
        r#"{"id":"top","name":"Report","rows":[{"id":"a","name":"Ann"},{"id":"b","name":"Bo"}]}"#,
    )]);
    let t = Target::parse("CREATE TABLE t (id TEXT NOT NULL, name TEXT NOT NULL) WITH (files = '*.json')").unwrap();
    let err = fit(&dir.path().join("both.json"), &t, Limits::default()).expect_err("both readings fit");
    let msg = format!("{err}");
    assert!(matches!(err, FitError::AmbiguousFrame { .. }), "{msg}");
    assert!(msg.contains("record = true") && msg.contains("pointer = \"/rows\""), "{msg}");
}

/// The corpus item's shape: the document is the record, and the one array
/// inside it (`games.nl.buyPrices`) is not the table. The declaration
/// eliminates the array, which is a proof — noted, not reviewed.
#[test]
fn a_record_that_alone_fits_is_proved_by_elimination() {
    let dir = record_pile(&[(
        "cap.json",
        r#"{"id":"cap","name":"1-up Cap","games":{"nl":{"sellPrice":{"value":80},"buyPrices":[{"currency":"bells","value":320}]}}}"#,
    )]);
    let t = Target::parse(
        "CREATE TABLE items (id TEXT NOT NULL, name TEXT NOT NULL, \
         sell BIGINT OPTIONS(matches = 'games', pointer = '/nl/sellPrice/value')) WITH (files = '*.json')",
    )
    .unwrap();
    let p = dir.path().join("cap.json");
    let fitted = fit(&p, &t, Limits::default()).expect("only the record fits");
    assert!(
        matches!(fitted.spec.extraction, tdy::spec::Extraction::Json { record: true, pointer: None, .. }),
        "{:?}",
        fitted.spec.extraction
    );
    assert!(fitted.spec.notes.iter().any(|n| n.contains("elimination")), "{:?}", fitted.spec.notes);
    assert!(fitted.review.is_none(), "{:?}", fitted.review);
    assert_eq!(ints_of(&fitted.spec, &p, 2), vec![Some(80)]);
}

/// The other way round: a document whose point is its array still reads the
/// array, now proved against the record reading too.
#[test]
fn an_array_that_alone_fits_still_wins_over_the_record() {
    let dir = record_pile(&[("rows.json", r#"{"meta":{"v":1},"rows":[{"id":"a"},{"id":"b"}]}"#)]);
    let t = Target::parse("CREATE TABLE t (id TEXT NOT NULL) WITH (files = '*.json')").unwrap();
    let p = dir.path().join("rows.json");
    let fitted = fit(&p, &t, Limits::default()).expect("only /rows fits");
    assert!(
        matches!(&fitted.spec.extraction, tdy::spec::Extraction::Json { record: false, pointer: Some(ptr), .. } if ptr == "/rows"),
        "{:?}",
        fitted.spec.extraction
    );
    assert!(fitted.review.is_none());
}

/// A document whose only array is empty is declined by the sniffer (zero
/// records is not one), but in a pile the target still decides: a target
/// naming the envelope's own keys fits it as one record.
#[test]
fn a_target_matching_the_record_still_fits_a_document_whose_array_is_empty() {
    let dir = record_pile(&[("status.json", r#"{"status":"ok","count":0,"rows":[]}"#)]);
    let t = Target::parse("CREATE TABLE s (status TEXT NOT NULL, count BIGINT NOT NULL) WITH (files = '*.json')").unwrap();
    let p = dir.path().join("status.json");
    let fitted = fit(&p, &t, Limits::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(fitted.spec.extraction, tdy::spec::Extraction::Json { record: true, .. }));
    assert_eq!(ints_of(&fitted.spec, &p, 1), vec![Some(0)]);
}

/// When no frame of a root object fits, the gap report is about the frame
/// the sniffer itself reads: for an API dump that is its array, whose real
/// gap (`amount` cannot parse "x") the record frame's "no column binds"
/// would bury.
#[test]
fn no_fitting_frame_reports_the_sniffers_own_frame() {
    let dir = record_pile(&[
        ("dump.json", r#"{"meta":{"v":1},"rows":[{"id":2,"amount":"x"}]}"#),
        ("rec.json", r#"{"id":"a","name":"x"}"#),
    ]);
    let t = Target::parse("CREATE TABLE t (id BIGINT NOT NULL, amount BIGINT NOT NULL) WITH (files = '*.json')").unwrap();
    let m = format!("{}", fit(&dir.path().join("dump.json"), &t, Limits::default()).unwrap_err());
    assert!(m.contains("`amount`") && m.contains("\"x\""), "{m}");
    assert!(!m.contains("no column of this file binds"), "{m}");

    // The other direction: where the sniffer reads the record, the report
    // is the record's — `id` holds "a", which is no BIGINT.
    let t = Target::parse("CREATE TABLE t (id BIGINT NOT NULL) WITH (files = '*.json')").unwrap();
    let m = format!("{}", fit(&dir.path().join("rec.json"), &t, Limits::default()).unwrap_err());
    assert!(m.contains("`id`") && m.contains("\"a\""), "{m}");
}

/// With no fitting frame, the report is the frame that bound the most
/// declared columns: here the record, whose real gap is a pointer landing
/// on the string "plain" — not the `/tags` array's "no column binds".
#[test]
fn no_fitting_frame_reports_the_frame_that_bound_most() {
    let dir = record_pile(&[(
        "one.json",
        r#"{"id":1,"a.b_c":5,"a_b":{"c":6},"g":"plain","deep":{"l1":{"l2":{"l3":{"l4":1}}}},"tags":["x"],"o":{"arr":[1]}}"#,
    )]);
    let t = Target::parse(
        "CREATE TABLE t (id BIGINT, g_nh_v BIGINT OPTIONS(matches = 'g', pointer = '/nh/v')) WITH (files = '*.json')",
    )
    .unwrap();
    let m = format!("{}", fit(&dir.path().join("one.json"), &t, Limits::default()).unwrap_err());
    assert!(m.contains("g_nh_v") && m.contains("plain"), "{m}");
    assert!(!m.contains("no column of this file binds"), "{m}");
}
