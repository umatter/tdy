//! `tdy mcp` — the same tool surface, spoken over the Model Context Protocol.
//!
//! For an agent doing data work the pitch is the same as for a human, only
//! sharper: parsing where a wrong value is structurally prevented, and where
//! failure comes back as an object it can act on — a gap names the column,
//! what was tried, the file's own header, and the one-line remedy.
//!
//! # Hand-rolled on purpose
//!
//! MCP's stdio transport is newline-delimited JSON-RPC 2.0 with five methods
//! this server needs. That is ~a page of protocol, and taking an SDK for it
//! would buy a dependency tree, an MSRV negotiation and a moving spec in
//! exchange for that page. The rule here is the project's usual one: bounded,
//! inspectable, ours.
//!
//! # The review gate survives the agent
//!
//! `fit` accepts an `accept` argument only when the server was started with
//! `--allow-accept`. By default an agent can *see* every review reason —
//! structured, with the evidence — but cannot approve one; the natural move
//! this leaves it is relaying the question to its human, which is exactly
//! what the gate means. Turning acceptance on is a statement that this
//! agent's operator takes those judgements on themselves.
//!
//! # Confinement
//!
//! Every path — tool arguments and the file references inside SQL — must
//! resolve under the `--root` directory (default: the working directory).
//! The check is on canonicalised paths, so `../` does not escape and neither
//! does a symlink out of the tree.
//!
//! # Discipline
//!
//! stdout carries protocol frames and nothing else; everything human goes to
//! stderr. Handlers therefore call the *pure* library functions (`fit_pile`,
//! `draft_target`, `run_query`) — never the printing command wrappers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::Config;

const PROTOCOL_VERSION: &str = "2024-11-05";
/// Rows a query returns inline. An agent that wants more should aggregate,
/// LIMIT, or ask for a file.
const DEFAULT_ROWS: usize = 200;
const MAX_ROWS: usize = 10_000;

pub struct McpServer {
    cfg: Config,
    root: PathBuf,
    allow_accept: bool,
}

pub async fn serve(cfg: Config, root: Option<PathBuf>, allow_accept: bool) -> Result<()> {
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = root
        .canonicalize()
        .with_context(|| format!("--root {} does not exist", root.display()))?;
    // Relative paths in tool arguments and in SQL resolve under the root.
    std::env::set_current_dir(&root)
        .with_context(|| format!("cannot enter --root {}", root.display()))?;
    let server = McpServer { cfg, root, allow_accept };

    eprintln!(
        "tdy mcp: serving over stdio (root {}, accept {})",
        server.root.display(),
        if server.allow_accept { "ENABLED" } else { "disabled — reviews go to a human" }
    );

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                respond(&mut stdout, &rpc_error(Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Notifications get no response, whatever they say.
        let Some(id) = id else { continue };

        let reply = match method {
            "initialize" => rpc_ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tdy", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            "ping" => rpc_ok(id, json!({})),
            "tools/list" => rpc_ok(id, json!({"tools": tool_list(server.allow_accept)})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match server.call(name, &args).await {
                    Ok(v) => rpc_ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": v.to_string()}],
                            "isError": false,
                        }),
                    ),
                    Err(e) => rpc_ok(
                        id,
                        json!({
                            "content": [{"type": "text", "text": format!("{e:#}")}],
                            "isError": true,
                        }),
                    ),
                }
            }
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        };
        respond(&mut stdout, &reply)?;
    }
    Ok(())
}

fn respond(out: &mut impl Write, v: &Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    out.write_all(line.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_list(allow_accept: bool) -> Value {
    let path_arg = |desc: &str| json!({"type": "string", "description": desc});
    let accept_desc = if allow_accept {
        "Members whose review reasons this call accepts, named as the fit report names \
         them: a path relative to the target, or `path#sheet` for one sheet of a workbook."
    } else {
        "DISABLED: acceptance is a human judgement, and this server was started without \
         --allow-accept. Relay the review reasons to your user instead."
    };
    json!([
        {
            "name": "sniff",
            "description": "Infer (or refresh) a file's parse spec and write its sidecar. \
                Returns confidence, notes (what tdy was unsure about), and the full spec.",
            "inputSchema": {"type": "object", "properties": {
                "path": path_arg("The data file."),
                "quick": {"type": "boolean", "description": "Skip the whole-file type check."},
                "force": {"type": "boolean", "description": "Re-infer even if a fresh sidecar exists."}
            }, "required": ["path"]},
        },
        {
            "name": "profile",
            "description": "Read-only: what each column of a file holds — non-empty and empty \
                counts, distinct values (a floor past 10000, with no top values), min and max \
                of the raw strings, the five most frequent values, and the shapes the values \
                take (`9999-99-99`, `Aa+`). Over the whole file unless `head` is given, in the \
                frame a fresh sidecar gives (or the sniffer's, heuristics only). Evidence for \
                a judgement such as a `matches` clause or a date order; writes nothing.",
            "inputSchema": {"type": "object", "properties": {
                "path": path_arg("The data file, or a member reference (`book.xlsx#Q1`, `report.csv#2`)."),
                "sheet": {"type": "string", "description": "One sheet of a workbook."},
                "rows": {"type": "string", "description": "One block of rows, 1-based and inclusive: `6-9`."},
                "head": {"type": "integer", "description": "Profile only the first N rows; the answer says complete: false."}
            }, "required": ["path"]},
        },
        {
            "name": "draft",
            "description": "Draft a CREATE TABLE target declaration from a pile of files: \
                every column name in every spelling seen, merged types, per-file presence. \
                A scaffold to edit — its comments list the judgements left to a human.",
            "inputSchema": {"type": "object", "properties": {
                "files": {"type": "array", "items": {"type": "string"},
                          "description": "The files the dataset should cover."}
            }, "required": ["files"]},
        },
        {
            "name": "fit",
            "description": "Fit every file a target's globs match onto its declared schema; \
                write sidecars, and the lock if ALL fit. Returns the full report: per member \
                its status, bindings, review reasons, and structured problems (each gap names \
                the column, what was tried, the file's own header, and the remedy).",
            "inputSchema": {"type": "object", "properties": {
                "target": path_arg("The .tdy.sql target file."),
                "dry_run": {"type": "boolean"},
                "propose": {"type": "boolean", "description": "For unmatched columns, list type-compatible candidates."},
                "accept": {"type": "array", "items": {"type": "string"}, "description": accept_desc}
            }, "required": ["target"]},
        },
        {
            "name": "check",
            "description": "The CI gate: is the dataset still exactly what its lock says? \
                Runs the same checks a query runs (drift, sidecars fresh, members conforming, \
                nothing awaiting review).",
            "inputSchema": {"type": "object", "properties": {
                "target": path_arg("The .tdy.sql target file.")
            }, "required": ["target"]},
        },
        {
            "name": "query",
            "description": "Run DataFusion SQL over messy('file') and dataset('t.tdy.sql') \
                references. Returns columns, types and rows (capped; use LIMIT/aggregate for \
                more than a preview).",
            "inputSchema": {"type": "object", "properties": {
                "sql": {"type": "string"},
                "max_rows": {"type": "integer", "description": "Row cap, default 200, max 10000."}
            }, "required": ["sql"]},
        },
        {
            "name": "validate",
            "description": "Prove a (possibly hand-edited) sidecar: spec valid, fingerprint \
                fresh, file still parses. With stamp=true, re-fingerprint after an edit — \
                the spec is checked against the file BEFORE being stamped.",
            "inputSchema": {"type": "object", "properties": {
                "path": path_arg("The data file (not the sidecar)."),
                "stamp": {"type": "boolean"}
            }, "required": ["path"]},
        },
    ])
}

impl McpServer {
    /// A path argument, confined to the root. The same check guards the
    /// executors themselves (`fileio::confine` inside `messy()`, `dataset()`
    /// and `fit_pile`), so a path that slips past this pre-check — or that
    /// changes between the check and the open — is refused again at open
    /// time by the code that opens it.
    fn scoped(&self, raw: &str) -> Result<PathBuf> {
        crate::fileio::confine(Path::new(raw), &self.root)
    }

    async fn call(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "sniff" => self.sniff(args).await,
            "draft" => self.draft(args),
            "fit" => self.fit(args).await,
            "check" => self.check(args),
            "query" => self.query(args).await,
            "validate" => self.validate(args),
            "profile" => self.profile(args),
            other => bail!("unknown tool {other:?}"),
        }
    }

    async fn sniff(&self, args: &Value) -> Result<Value> {
        let path = self.scoped(str_arg(args, "path")?)?;
        let quick = args["quick"].as_bool().unwrap_or(false);
        let force = args["force"].as_bool().unwrap_or(false);
        let prepared = crate::provider::ensure_sidecar_opts(
            &path,
            &self.cfg,
            None,
            force,
            crate::sniff::SniffOpts { verify: !quick },
        )
        .await?;
        crate::provider::sniff_json_value(&path, &prepared)
    }

    fn draft(&self, args: &Value) -> Result<Value> {
        let files = args["files"]
            .as_array()
            .ok_or_else(|| anyhow!("`files` must be an array of paths"))?
            .iter()
            .map(|f| {
                self.scoped(f.as_str().ok_or_else(|| anyhow!("`files` entries must be strings"))?)
            })
            .collect::<Result<Vec<_>>>()?;
        // Relative to the served root, where the agent names every path.
        let sql = crate::draft::draft_target_in(&files, Some(&self.root), self.cfg.limits)?;
        Ok(json!({"sql": sql}))
    }

    async fn fit(&self, args: &Value) -> Result<Value> {
        let target = self.scoped(str_arg(args, "target")?)?;
        let accept: Vec<PathBuf> = args["accept"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect())
            .unwrap_or_default();
        if !accept.is_empty() && !self.allow_accept {
            bail!(
                "acceptance is a human judgement — this member's review reasons describe a \
                 decision tdy cannot check (and neither can you). Relay them to your user; \
                 the server operator can start `tdy mcp --allow-accept` to delegate \
                 acceptance to this agent."
            );
        }
        let report = crate::report::fit_pile(
            &target,
            &self.cfg,
            crate::report::FitOpts {
                dry_run: args["dry_run"].as_bool().unwrap_or(false),
                accept: &accept,
                propose: args["propose"].as_bool().unwrap_or(false),
                // stdout is protocol here, so nothing is narrated; the
                // report itself is the answer.
                progress: None,
                // A target may declare globs that reach outside the served
                // tree; a confined fit refuses them instead of writing
                // sidecars out there.
                root: Some(&self.root),
            },
        )
        .await?;
        Ok(serde_json::to_value(report)?)
    }

    fn check(&self, args: &Value) -> Result<Value> {
        let target_path = self.scoped(str_arg(args, "target")?)?;
        let target = crate::target::Target::load(&target_path)?;
        if crate::lockfile::Lock::load(&target_path)?.is_none() {
            return Ok(json!({
                "target": target.name, "ready": false,
                "reason": "no lock — run the fit tool first",
            }));
        }
        Ok(match crate::dataset::resolve(&target_path, self.cfg.limits, Some(&self.root)) {
            Ok(resolved) => json!({
                "target": target.name, "ready": true,
                "members": resolved.members.iter().map(|m| m.rel.clone()).collect::<Vec<_>>(),
            }),
            Err(e) => json!({
                "target": target.name, "ready": false, "reason": format!("{e:#}"),
            }),
        })
    }

    async fn query(&self, args: &Value) -> Result<Value> {
        let sql = str_arg(args, "sql")?;
        let max_rows = args["max_rows"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_ROWS)
            .min(MAX_ROWS);

        // The SQL names files; the rooted runner confines them — before the
        // inference pre-pass, and again inside `messy()`/`dataset()` at open
        // time, where a dataset's lock members are confined too.
        let (schema, batches) =
            crate::provider::run_query_rooted(sql, &self.cfg, false, Some(self.root.clone()))
                .await?;
        let columns: Vec<Value> = schema
            .fields()
            .iter()
            .map(|f| json!({"name": f.name(), "type": format!("{}", f.data_type())}))
            .collect();

        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        let mut rows: Vec<Value> = Vec::new();
        'outer: for b in &batches {
            for i in 0..b.num_rows() {
                if rows.len() >= max_rows {
                    break 'outer;
                }
                let row: Vec<Value> = (0..b.num_columns())
                    .map(|c| {
                        let col = b.column(c);
                        if col.is_null(i) {
                            Value::Null
                        } else {
                            datafusion::arrow::util::display::array_value_to_string(col, i)
                                .map(Value::String)
                                .unwrap_or(Value::Null)
                        }
                    })
                    .collect();
                rows.push(Value::Array(row));
            }
        }
        Ok(json!({
            "columns": columns,
            "rows": rows,
            "row_count": total,
            "truncated": total > rows.len(),
        }))
    }

    /// Read-only. A member reference is split by the sidecar it names
    /// (`sidecar::resolve_ref`) and the data file it resolves to confined
    /// like any other path — before anything opens it.
    fn profile(&self, args: &Value) -> Result<Value> {
        let raw = str_arg(args, "path")?;
        let joined = self.root.join(raw);
        let (file, sheet, region) = crate::sidecar::resolve_ref(&joined)?;
        let file = crate::fileio::confine(&file, &self.root)?;
        let rows = args["rows"].as_str().map(crate::profile::parse_rows).transpose()?;
        let req = crate::profile::Request {
            sheet: args["sheet"].as_str().map(String::from),
            rows,
            head: args["head"].as_u64(),
        };
        let mut p = crate::profile::profile_member(&file, sheet, region, &req, self.cfg.limits)?;
        p.path = raw.to_string();
        Ok(serde_json::to_value(p)?)
    }

    fn validate(&self, args: &Value) -> Result<Value> {
        let path = self.scoped(str_arg(args, "path")?)?;
        let stamp = args["stamp"].as_bool().unwrap_or(false);
        // The command wrapper prints; here the outcome IS the return value.
        match crate::provider::validate_quiet(&path, &self.cfg, stamp) {
            Ok(notes) => Ok(json!({"ok": true, "stamped": stamp, "notes": notes})),
            Err(e) => Ok(json!({"ok": false, "error": format!("{e:#}")})),
        }
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .ok_or_else(|| anyhow!("missing required string argument `{key}`"))
}
