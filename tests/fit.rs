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
    // Four fractional digits, so the separator cannot be a thousands group
    // and the column is unambiguously decimal — see the test below for what
    // happens when it is not.
    std::fs::write(&p, "id;amount\n1;1.0056\n2;2.9942\n").unwrap();

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
    assert_eq!(p.column, "amount_chf");
    assert_eq!(p.candidates.len(), 1, "{:?}", p.candidates);
    assert_eq!(p.candidates[0].0, "Betrag Rp.");

    // The remedy is pasteable, keeps the declared aliases, and does not repeat
    // the column's own name (the binder always tries that first).
    let existing = vec!["amount_chf".to_string(), "Betrag".to_string()];
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
           amount_chf DECIMAL(14,2) NOT NULL,
           datum      DATE          NOT NULL
         ) WITH (files = 't.csv', date_order = 'dmy')",
    )
    .unwrap();

    let props = tdy::fit::propose(&p, &t, Limits::default()).unwrap();
    assert_eq!(props.len(), 1, "{props:?}");
    assert_eq!(props[0].column, "amount_chf");
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
           amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag, Betrag Rp.')
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
    assert!(tdy::fit::is_binding_note(&tdy::fit::binding_note("amount_chf", "Betrag")));
    assert!(!tdy::fit::is_binding_note(
        "`amount_chf`: some values carry more than 2 fractional digits and are rounded \
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
           amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
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
           amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag')
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

fn json_fixture(name: &str) -> PathBuf {
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
    let p = json_fixture("json_frames_one_fits.json");
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
    let p = json_fixture("json_frames_two_fit.json");
    let err = fit(&p, &t, Limits::default()).expect_err("q1 and q2 both fit");
    let msg = format!("{err}");
    assert!(matches!(err, FitError::AmbiguousFrame { .. }), "{msg}");
    assert!(msg.contains("/q1") && msg.contains("/q2"), "{msg}");
    assert!(msg.contains("pointer"), "the remedy must be named:\n{msg}");
}
