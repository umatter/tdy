//! The model as frame proposer — offline.
//!
//! The contract under test: when deterministic planning fails and a backend
//! is configured, `fit::plan` asks the model for the *frame only*, proves
//! everything downstream (binding, types, conformance, dry run, whole-file
//! verification), and marks the result for review — because the one thing no
//! gate can prove is that the model's frame is the only reading of the file.
//!
//! The "model" here is a hand-rolled HTTP server on a loopback port speaking
//! just enough of the OpenAI chat-completions wire format to return a canned
//! spec. No network, no tokens, runs in CI; `tests/live_backend.rs` is where
//! a real model answers.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;

use tdy::config::{Backend, Config};
use tdy::target::Target;

/// Serve `content` as the assistant message for every POST, forever (the
/// weakening ladder may retry). Returns the base URL.
fn mock_backend(content: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = serde_json::json!({
                "choices": [{"message": {"content": content}}]
            })
            .to_string();
            // Drain the request headers + body enough to be polite.
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn cfg_for(base_url: String) -> Config {
    let mut cfg = Config::default();
    cfg.backend = Backend::Local;
    cfg.base_url = base_url;
    cfg.model = "mock-model".into();
    cfg
}

/// A log file no delimiter sniff can frame: the model proposes a `lines`
/// regex, and tdy proves the rest.
#[tokio::test]
async fn a_model_proposed_frame_is_proved_and_gated_behind_review() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("app.log");
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
    std::fs::write(&p, body).unwrap();

    // What the "model" answers: a full ParseSpec whose frame is a lines
    // regex. Its columns are deliberately all-text placeholders — the planner
    // must discard them and re-derive from the declaration.
    let proposed = serde_json::json!({
        "extraction": {
            "format": "lines",
            "pattern": r"^\[(?P<day>\d{4}-\d{2}-\d{2}) [^\]]*\] region=(?P<region>\S+) amount=(?P<amount>\S+) .*$"
        },
        "columns": [
            {"name": "day", "dtype": {"type": "utf8"}},
            {"name": "region", "dtype": {"type": "utf8"}},
            {"name": "amount", "dtype": {"type": "utf8"}}
        ],
        "confidence": 0.9
    });
    let cfg = cfg_for(mock_backend(proposed.to_string()));

    let target = Target::parse(
        "CREATE TABLE bookings (
           day    DATE          NOT NULL,
           region TEXT          NOT NULL,
           amount DECIMAL(14,2) NOT NULL
         ) WITH (files = '*.log')",
    )
    .unwrap();

    // Deterministic planning must fail on its own first…
    let det = tdy::fit::fit(&p, &target, cfg.limits);
    assert!(det.is_err(), "if the sniffer can frame this, the test proves nothing");

    // …and the model path must fit it, typed from the declaration.
    let planned = tdy::fit::plan(&p, &target, &cfg).await.expect("the frame fits");
    assert_eq!(planned.method, tdy::spec::InferenceMethod::Llm);
    assert_eq!(planned.model.as_deref(), Some("mock-model"));

    let review = planned.fitted.review.as_deref().expect("a model frame is a judgement");
    assert!(review.contains("mock-model"), "{review}");
    assert!(review.contains("lines"), "the frame must be described: {review}");

    let spec = &planned.fitted.spec;
    assert_eq!(spec.columns.len(), 3);
    assert!(matches!(spec.columns[0].dtype, tdy::spec::DType::Date { .. }));
    let b = tdy::provider::spec_to_batch(spec, &p).unwrap();
    assert_eq!(b.num_rows(), 4);
    let amounts = b
        .column(2)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Decimal128Array>()
        .unwrap();
    let total: i128 = (0..amounts.len()).map(|i| amounts.value(i)).sum();
    assert_eq!(total, 66000, "sum must be 660.00");
}

/// A proven ambiguity is settled by a declaration, never by a model: the
/// two-fitting-arrays document stays refused even with a backend configured.
#[tokio::test]
async fn a_model_is_never_asked_to_resolve_a_proven_ambiguity() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("json_frames_two_fit.json");
    // A backend that would happily answer — the test is that nobody calls it.
    let cfg = cfg_for(mock_backend("{}".into()));
    let target = Target::parse(
        "CREATE TABLE orders (
           day    DATE          NOT NULL,
           region TEXT          NOT NULL,
           amount DECIMAL(14,2) NOT NULL
         ) WITH (files = '*.json')",
    )
    .unwrap();
    let err = tdy::fit::plan(&p, &target, &cfg).await.expect_err("must stay refused");
    assert!(
        matches!(err, tdy::fit::FitError::AmbiguousFrame { .. }),
        "{err}"
    );
}
