//! Live inference-tier tests. **Skipped unless you ask for them.**
//!
//! Everything else in the suite runs with `backend = none`, which is what
//! makes CI free and offline. But that leaves the tier-2 wire formats — the
//! request shape, the constrained-decoding ladder, `stop_reason` handling,
//! the retry loop — verified only structurally. These tests close that gap by
//! actually calling a model, and they cost real money, so they run only when
//! you name a model:
//!
//! ```text
//! export OPENROUTER_API_KEY=...
//! TDY_LIVE_MODEL=google/gemini-2.5-flash cargo test --test live_backend -- --nocapture
//! ```
//!
//! Set `TDY_LIVE_BACKEND` (default `openrouter`) to point at a different
//! backend — `local` against a llama.cpp or Ollama server costs nothing:
//!
//! ```text
//! TDY_LIVE_BACKEND=local TDY_LIVE_MODEL=qwen2.5-coder:32b \
//!   TDY_BASE_URL=http://localhost:11434 cargo test --test live_backend
//! ```
//!
//! These are capability tests as much as integration tests, and the bar is a
//! real one: the spec the model writes must produce the *same answer* as the
//! hand-written reference spec in `tests/e2e.rs`. A model that cannot read a
//! two-row merged header with German month columns cannot be trusted with
//! your spreadsheets either.
//!
//! Observed on `testdata/umsatz.xlsx`: `google/gemini-2.5-flash` and
//! `anthropic/claude-sonnet-4.5` pass; `openai/gpt-4o-mini` does not. Model
//! output is not deterministic even at temperature 0, so a marginal model
//! will pass intermittently — that is information, not flakiness.

use std::path::{Path, PathBuf};
use std::process::Command;

fn live_model() -> Option<String> {
    std::env::var("TDY_LIVE_MODEL").ok().filter(|v| !v.trim().is_empty())
}

fn live_backend() -> String {
    std::env::var("TDY_LIVE_BACKEND").unwrap_or_else(|_| "openrouter".into())
}

/// Copy a fixture somewhere writable: inference writes a sidecar next to it.
fn stage(dir: &Path, name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    assert!(src.exists(), "missing fixture {name} — run `python3 gen_fixtures.py`");
    let dst = dir.join(src.file_name().unwrap());
    std::fs::copy(&src, &dst).expect("copy fixture");
    dst
}

fn tdy(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tdy")).args(args).output().expect("run tdy")
}

fn infer(path: &Path, model: &str) -> std::process::Output {
    // A hard file deserves a fair number of correction rounds: each one
    // carries the exact failure back to the model, so the loop converges.
    // The shipped default of 2 is a cost decision, not a capability one.
    Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args([
            "sniff",
            path.to_str().unwrap(),
            "--force",
            "--backend",
            &live_backend(),
            "--model",
            model,
        ])
        .env("TDY_MAX_RETRIES", std::env::var("TDY_MAX_RETRIES").unwrap_or_else(|_| "5".into()))
        .output()
        .expect("run tdy")
}

/// The canonical tier-2 case, judged against a known-good answer.
///
/// `umsatz.xlsx` has a three-row title block, a two-row merged header, a year
/// merged across four month columns, vertically merged Region cells, an
/// interleaved subtotal row, a Total footer, Swiss number formatting and
/// German month abbreviations chrono cannot parse. Heuristics score it ~0.60
/// and say why.
///
/// The reference answer comes from the hand-written spec in tests/e2e.rs:
/// sixteen money values totalling 21_244.25, with the subtotal and Total rows
/// excluded.
///
/// Deliberately *not* asserted: the shape. Long form (unpivoted, one row per
/// month) and wide form (a column per month) are both faithful readings of
/// this sheet, and models legitimately choose differently —
/// gemini-2.5-flash unpivots, claude-sonnet-4.5 does not. What is not
/// negotiable is the arithmetic: the right numbers, and no summary row
/// counted as data.
#[test]
fn tier_two_matches_the_hand_written_reference_spec() {
    let Some(model) = live_model() else {
        eprintln!("skipping: set TDY_LIVE_MODEL to run the live backend tests");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let p = stage(dir.path(), "umsatz.xlsx");

    let out = infer(&p, &model);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "inference failed with {model}:\n{err}\n\
         (a failure here is a real signal — see this file's module docs)"
    );
    // The egress notice is part of the contract: a remote backend must say
    // how much of the file is leaving.
    if live_backend() != "local" {
        assert!(err.contains("sending"), "no data-egress notice was printed:\n{err}");
    }

    // Provenance must record what produced the spec, so a sidecar committed
    // to a repo can be audited later.
    let sidecar = std::fs::read_to_string(tdy::sidecar::sidecar_path(&p)).unwrap();
    assert!(sidecar.contains("method = \"llm\""), "spec not attributed to the model");
    assert!(sidecar.contains(&format!("model = \"{model}\"")));
    assert!(sidecar.contains("prompt_version"));

    // The answer itself, read out of the CSV so it does not depend on what
    // the model chose to call the columns.
    let csv = dir.path().join("out.csv");
    let q = tdy(&[
        "query",
        &format!("SELECT * FROM messy('{}')", p.display()),
        "-f",
        "-o",
        csv.to_str().unwrap(),
    ]);
    assert!(q.status.success(), "query failed:\n{}", String::from_utf8_lossy(&q.stderr));
    let text = std::fs::read_to_string(&csv).unwrap();
    let body: String = text.lines().skip(1).collect::<Vec<_>>().join("\n");

    // No summary row may have been read as data.
    for ghost in ["Zwischensumme", "Total", "zwischensumme"] {
        assert!(
            !body.contains(ghost),
            "the {ghost} row survived into the data:\n{text}"
        );
    }

    // The money, whatever shape it is in. Dates are ISO strings and do not
    // parse as f64, so this picks up the amounts and nothing else.
    let values: Vec<f64> = body
        .split([',', '\n'])
        .filter_map(|c| c.trim().parse::<f64>().ok())
        .filter(|v| *v > 1.0) // not a year fragment or an index
        .collect();
    let sum: f64 = values.iter().sum();
    assert!(
        (sum - 21_244.25).abs() < 0.005,
        "the amounts sum to {sum}, reference is 21244.25 — a subtotal row survived, \
         or a value was mis-parsed. Got {} values:\n{text}",
        values.len()
    );

    // Frozen re-runs must reproduce it exactly, with no further inference.
    let a = tdy(&["query", &format!("SELECT * FROM messy('{}')", p.display()), "-f"]);
    let b = tdy(&["query", &format!("SELECT * FROM messy('{}')", p.display()), "-f"]);
    assert_eq!(a.stdout, b.stdout, "frozen re-runs disagreed");
}

/// The safety property that matters more than any success: when the model
/// gets it wrong, tdy must refuse the spec rather than write it. The gate is
/// validate() plus a dry run against the real file, so a spec that cannot
/// parse its own file never reaches a sidecar.
#[test]
fn a_spec_the_model_gets_wrong_is_never_written() {
    let Some(model) = live_model() else {
        eprintln!("skipping: set TDY_LIVE_MODEL to run the live backend tests");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    // A decorated fixed-width report: title block, ruler lines, group headers,
    // an overflowed numeric field. Models frequently get the column offsets
    // slightly wrong here.
    let p = stage(dir.path(), "logs_fixed_width_report_ascii.txt");

    let out = infer(&p, &model);
    let sc = tdy::sidecar::sidecar_path(&p);
    if out.status.success() {
        // If it succeeded, the spec must actually work on the whole file.
        let q = tdy(&["query", &format!("SELECT count(*) FROM messy('{}')", p.display()), "-f"]);
        assert!(
            q.status.success(),
            "a spec was written that cannot parse its own file:\n{}",
            String::from_utf8_lossy(&q.stderr)
        );
    } else {
        // If it failed, nothing may have been persisted, and the error must
        // say what went wrong rather than merely that something did.
        assert!(!sc.exists(), "a rejected spec was still written to {}", sc.display());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("could not produce a working spec"),
            "unhelpful failure:\n{err}"
        );
        eprintln!("note: {model} did not solve the fixed-width report — reported cleanly");
    }
}

/// The frame proposer against a real model: a log file no delimiter sniff
/// can frame, fitted onto a declared table via `tdy fit`. The model's only
/// contribution is the frame (a `lines` regex); binding, typing, conformance
/// and the whole-file check are all proved on this side, and the member is
/// marked for review because a model-chosen frame is a judgement.
#[test]
fn a_real_model_can_propose_a_frame_that_the_gates_then_prove() {
    let Some(model) = live_model() else {
        eprintln!("skipping: set TDY_LIVE_MODEL to run the live backend tests");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("bookings.log");
    let mut body = String::new();
    for (i, (day, region)) in [
        ("2025-08-05", "Ost"),
        ("2025-08-12", "West"),
        ("2025-08-19", "Nord"),
        ("2025-08-26", "Sued"),
    ]
    .iter()
    .enumerate()
    {
        body.push_str(&format!(
            "[{day} 10:0{i}:00] region={region} amount={}.00 msg=\"booked ok\"\n",
            150 + 10 * i
        ));
    }
    std::fs::write(&log, body).unwrap();
    let target = dir.path().join("bookings.tdy.sql");
    std::fs::write(
        &target,
        "CREATE TABLE bookings (\n\
         \x20 day    DATE          NOT NULL,\n\
         \x20 region TEXT          NOT NULL,\n\
         \x20 amount DECIMAL(14,2) NOT NULL\n\
         )\nWITH (files = '*.log');",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tdy"))
        .args([
            "fit",
            target.to_str().unwrap(),
            "--backend",
            &live_backend(),
            "--model",
            &model,
        ])
        .env("TDY_MAX_RETRIES", std::env::var("TDY_MAX_RETRIES").unwrap_or_else(|_| "5".into()))
        .output()
        .expect("run tdy");
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "fit failed:\n{text}{err}");
    assert!(text.contains("REVIEW"), "a model frame must need review:\n{text}");

    // Accept and query: the numbers, not the shape, are the contract.
    assert!(tdy(&["fit", target.to_str().unwrap(), "--accept", "bookings.log"])
        .status
        .success());
    let q = format!("SELECT sum(amount) FROM dataset('{}')", target.display());
    let out = tdy(&["query", &q]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("660.00"), "wrong total:\n{text}");
}
