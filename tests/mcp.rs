//! `tdy mcp` — the protocol, the confinement, and the review gate, exercised
//! the way a client would: a subprocess speaking newline-delimited JSON-RPC
//! over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("drifting_exports")
}

struct Server {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Server {
    fn start(root: &Path, allow_accept: bool) -> Server {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tdy"));
        cmd.arg("mcp").arg("--root").arg(root);
        if allow_accept {
            cmd.arg("--allow-accept");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tdy mcp");
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut s = Server { child, reader, next_id: 0 };
        let init = s.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "tdy");
        s.notify("notifications/initialized");
        s
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params,
        });
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("server reply");
        serde_json::from_str(&line).expect("reply is JSON")
    }

    fn notify(&mut self, method: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc": "2.0", "method": method})).unwrap();
        stdin.flush().unwrap();
    }

    /// Call a tool; returns (payload, isError).
    fn call(&mut self, name: &str, args: serde_json::Value) -> (serde_json::Value, bool) {
        let r = self.request("tools/call", serde_json::json!({"name": name, "arguments": args}));
        let is_err = r["result"]["isError"].as_bool().unwrap_or(false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        let payload = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        (payload, is_err)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn staged() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for e in std::fs::read_dir(corpus()).unwrap().flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if p.is_file() && !n.ends_with(".tdy.toml") && !n.ends_with(".tdy.lock") {
            std::fs::copy(&p, dir.path().join(&n)).unwrap();
        }
    }
    dir
}

/// The whole agent loop against the corpus: list the tools, fit the pile,
/// read the structured report, query the dataset.
#[test]
fn an_agent_can_fit_and_query_a_pile_over_mcp() {
    let dir = staged();
    let mut s = Server::start(dir.path(), false);

    let tools = s.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["sniff", "draft", "fit", "check", "query", "validate"]);

    let (report, err) = s.call("fit", serde_json::json!({"target": "sales_ok.tdy.sql"}));
    assert!(!err, "{report:#}");
    assert_eq!(report["failed"], 0);
    assert!(report["lock_written"].is_string());

    let (res, err) = s.call(
        "query",
        serde_json::json!({
            "sql": "SELECT count(*) n, sum(amount_chf) total FROM dataset('sales_ok.tdy.sql')",
        }),
    );
    assert!(!err, "{res:#}");
    assert_eq!(res["rows"][0][0], "36");
    assert_eq!(res["rows"][0][1], "57340.00");

    let (chk, err) = s.call("check", serde_json::json!({"target": "sales_ok.tdy.sql"}));
    assert!(!err);
    assert_eq!(chk["ready"], true, "{chk:#}");
}

/// The review gate survives the agent: without --allow-accept the review
/// reasons are visible and acceptance is refused with the reason why.
#[test]
fn acceptance_is_refused_unless_the_operator_delegated_it() {
    let dir = staged();
    // A target the Rappen file joins only via a value-changing judgement.
    std::fs::write(
        dir.path().join("rappen.tdy.sql"),
        "CREATE TABLE rappen (\n\
         \x20 month      DATE          NOT NULL OPTIONS(matches = 'Datum'),\n\
         \x20 region     TEXT          NOT NULL OPTIONS(matches = 'Region'),\n\
         \x20 amount_chf DECIMAL(14,2) NOT NULL OPTIONS(matches = 'Betrag Rp.')\n\
         )\nWITH (files = '2025-07.csv', date_order = 'dmy');",
    )
    .unwrap();
    let sc = dir.path().join("2025-07.csv.tdy.toml");
    std::fs::write(
        &sc,
        r#"spec_version = 1
[source]
path = "2025-07.csv"
blake3 = "0"
bytes = 0
[provenance]
method = "manual"
tool_version = "0.1.0"
created_at = "2026-01-01T00:00:00Z"
[spec]
[spec.extraction]
format = "delimited"
delimiter = ";"
quote = '"'
encoding = "windows-1252"
ragged = "pad_nulls"
[[spec.transforms]]
op = "promote_header"
rows = 1
join = " "
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
"#,
    )
    .unwrap();

    let mut s = Server::start(dir.path(), false);
    let (v, err) = s.call("validate", serde_json::json!({"path": "2025-07.csv", "stamp": true}));
    assert!(!err && v["ok"] == true, "{v:#}");

    // The report shows the judgement, structured.
    let (report, err) = s.call("fit", serde_json::json!({"target": "rappen.tdy.sql"}));
    assert!(!err, "{report:#}");
    assert_eq!(report["needs_review"], 1, "{report:#}");
    let m = &report["members"][0];
    assert_eq!(m["status"], "needs_review");
    assert!(m["review"].as_str().unwrap().contains("decimal_shift"));

    // Accepting it is refused, with the delegation path named.
    let (msg, err) = s.call(
        "fit",
        serde_json::json!({"target": "rappen.tdy.sql", "accept": ["2025-07.csv"]}),
    );
    assert!(err, "acceptance must be refused: {msg:#}");
    assert!(msg.as_str().unwrap().contains("human judgement"), "{msg:#}");
    assert!(msg.as_str().unwrap().contains("--allow-accept"), "{msg:#}");
    drop(s);

    // With the flag, the operator has delegated: acceptance works.
    let mut s = Server::start(dir.path(), true);
    let (report, err) = s.call(
        "fit",
        serde_json::json!({"target": "rappen.tdy.sql", "accept": ["2025-07.csv"]}),
    );
    assert!(!err, "{report:#}");
    assert_eq!(report["needs_review"], 0, "{report:#}");
    let (res, err) = s.call(
        "query",
        serde_json::json!({"sql": "SELECT sum(amount_chf) FROM dataset('rappen.tdy.sql')"}),
    );
    assert!(!err, "{res:#}");
    assert_eq!(res["rows"][0][0], "6860.00", "the shift must be applied exactly");
}

/// Confinement: no path — argument or SQL reference — escapes --root.
#[test]
fn every_path_is_confined_to_the_root() {
    let dir = staged();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret.csv"), "a,b\n1,2\n").unwrap();
    let mut s = Server::start(dir.path(), false);

    let (msg, err) = s.call("sniff", serde_json::json!({"path": "../secret.csv"}));
    assert!(err, "{msg:#}");

    let abs = outside.path().join("secret.csv");
    let (msg, err) = s.call(
        "query",
        serde_json::json!({"sql": format!("SELECT * FROM messy('{}')", abs.display())}),
    );
    assert!(err, "an absolute path outside root must be refused: {msg:#}");
    assert!(msg.as_str().unwrap().contains("outside"), "{msg:#}");

    // Row cap: 36 rows exist, 5 come back, and the truncation is declared.
    let (report, _) = s.call("fit", serde_json::json!({"target": "sales_ok.tdy.sql"}));
    assert_eq!(report["failed"], 0);
    let (res, err) = s.call(
        "query",
        serde_json::json!({
            "sql": "SELECT * FROM dataset('sales_ok.tdy.sql')", "max_rows": 5,
        }),
    );
    assert!(!err);
    assert_eq!(res["rows"].as_array().unwrap().len(), 5);
    assert_eq!(res["row_count"], 36);
    assert_eq!(res["truncated"], true);
}
