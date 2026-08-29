//! Tier-2: LLM spec inference.
//!
//! The model's job is narrow: look at a rendered sample and emit a ParseSpec
//! *body* as JSON. It never parses data, never sees the whole file, and its
//! output is grammar-constrained where the backend supports it:
//!
//! - OpenAI-compatible servers (llama.cpp, vLLM, Ollama): `response_format:
//!   json_schema` with the schema derived from the ParseSpec structs, then a
//!   ladder of weaker fallbacks for servers that reject it.
//! - Anthropic: a forced tool call whose `input_schema` is the same schema.
//!
//! Verification loop (per attempt): deserialize (deny_unknown_fields) ->
//! spec.validate() -> engine::dry_run on the actual file. Any failure's
//! message goes back to the model; max_retries caps the loop.
//!
//! Two failure classes are kept apart. A *spec* problem is the model's to
//! fix, so it is fed back as text. A *transport* problem (timeout, 503) is
//! not, so the same prompt is retried instead of blaming the model for the
//! network.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;

use crate::config::{Backend, Config};
use crate::engine;
use crate::sample::FileSample;
use crate::sniff::SniffResult;
use crate::spec::ParseSpec;

pub const PROMPT_VERSION: &str = "infer-v3";

/// Feedback text sent back to the model is capped: parse errors quote the
/// offending value, and those come from the whole probed head of the file
/// rather than from the bounded sample.
const MAX_FEEDBACK_CHARS: usize = 2000;

/// Transport retries, separate from the spec-correction retries.
const TRANSPORT_ATTEMPTS: u32 = 3;

/// Cap on the draft spec included in the prompt: it is derived from more of
/// the file than the sample shows.
const MAX_DRAFT_CHARS: usize = 8000;

#[async_trait]
pub trait SpecInferencer: Send + Sync {
    async fn complete(&self, system: &str, user: &str, schema: &serde_json::Value)
        -> Result<String>;
    fn model_name(&self) -> String;
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10).min(timeout))
        .build()
        .context("building HTTP client")
}

pub fn make_inferencer(cfg: &Config) -> Result<Box<dyn SpecInferencer>> {
    match cfg.backend {
        Backend::None => bail!(
            "heuristics were not confident and no LLM backend is configured.\n\
             Options: run `tdy sniff <file>` and edit the sidecar by hand, or\n\
             configure a backend (tdy config init; backend = \"local\" keeps\n\
             everything on this machine)."
        ),
        Backend::Local => Ok(Box::new(OpenAiCompatible {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key: read_key(&cfg.api_key_env),
            extra_headers: Vec::new(),
            client: http_client(cfg.http_timeout)?,
        })),
        Backend::OpenRouter => {
            let key = read_key(&cfg.api_key_env).ok_or_else(|| {
                anyhow!(
                    "environment variable {} is not set (it must hold your OpenRouter \
                     API key; create one at https://openrouter.ai/keys)",
                    cfg.api_key_env
                )
            })?;
            Ok(Box::new(OpenAiCompatible {
                base_url: cfg.base_url.trim_end_matches('/').to_string(),
                model: cfg.model.clone(),
                api_key: Some(key),
                // OpenRouter attributes requests by these; they are optional
                // and carry no file content.
                extra_headers: vec![
                    ("HTTP-Referer", "https://github.com/umatter/tdy".to_string()),
                    ("X-Title", "tdy".to_string()),
                ],
                client: http_client(cfg.http_timeout)?,
            }))
        }
        Backend::Anthropic => {
            let key = read_key(&cfg.api_key_env).ok_or_else(|| {
                anyhow!(
                    "environment variable {} is not set (it must hold the API key)",
                    cfg.api_key_env
                )
            })?;
            Ok(Box::new(Anthropic {
                model: cfg.model.clone(),
                api_key: key,
                client: http_client(cfg.http_timeout)?,
            }))
        }
    }
}

fn read_key(env: &str) -> Option<String> {
    if env.is_empty() {
        None
    } else {
        std::env::var(env).ok().filter(|v| !v.trim().is_empty())
    }
}

// ---------------------------------------------------------------------------
// Orchestration: draft -> model -> deserialize -> validate -> dry run
// ---------------------------------------------------------------------------

pub struct Inferred {
    pub spec: ParseSpec,
    pub model: String,
}

pub async fn infer_spec(
    cfg: &Config,
    path: &Path,
    sample: &FileSample,
    draft: Option<&SniffResult>,
    hint: Option<&str>,
) -> Result<Inferred> {
    let backend = make_inferencer(cfg)?;
    let schema = ParseSpec::json_schema();
    let system = SYSTEM_PROMPT.to_string();
    let mut feedback: Option<Feedback> = None;
    let mut attempts = 0u32;

    let schema_text = serde_json::to_string(&schema).unwrap_or_default();
    loop {
        attempts += 1;
        let user = build_user_prompt(sample, draft, hint, feedback.as_ref(), &schema_text);
        let raw = complete_with_transport_retries(backend.as_ref(), &system, &user, &schema).await?;
        let json_text = strip_fences(&raw);

        let problem = match serde_json::from_str::<ParseSpec>(json_text) {
            Err(e) => format!("your JSON did not deserialize: {e}"),
            Ok(spec) => match spec.validate() {
                Err(errs) => format!("the spec failed validation:\n- {}", errs.join("\n- ")),
                Ok(()) => match engine::dry_run(&spec, path, cfg.limits) {
                    Err(e) => format!(
                        "the spec deserialized and validated, but failed when \
                         executed against the actual file:\n{e:#}"
                    ),
                    Ok(_) => {
                        return Ok(Inferred { spec, model: backend.model_name() });
                    }
                },
            },
        };

        if attempts > cfg.max_retries {
            bail!(
                "the model could not produce a working spec after {} attempt(s). \
                 Last problem:\n{}\n\nRun `tdy sniff {} --no-llm` to see the heuristic \
                 draft and edit the sidecar by hand.",
                attempts,
                truncate(&problem, MAX_FEEDBACK_CHARS),
                path.display()
            );
        }
        feedback = Some(Feedback {
            problem: truncate(&problem, MAX_FEEDBACK_CHARS),
            previous: json_text.to_string(),
        });
    }
}

struct Feedback {
    problem: String,
    previous: String,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n[... message truncated ...]")
}

/// A timeout or a 503 is not something the model can fix by rewriting its
/// spec, so the same prompt goes back out rather than burning a correction
/// round.
async fn complete_with_transport_retries(
    backend: &dyn SpecInferencer,
    system: &str,
    user: &str,
    schema: &serde_json::Value,
) -> Result<String> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=TRANSPORT_ATTEMPTS {
        match backend.complete(system, user, schema).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt < TRANSPORT_ATTEMPTS && is_retryable(&e) {
                    let backoff = Duration::from_millis(500 * u64::from(attempt));
                    eprintln!(
                        "note: inference request failed ({}); retrying in {:?}",
                        first_line(&format!("{e:#}")),
                        backoff
                    );
                    tokio::time::sleep(backoff).await;
                    last = Some(e);
                    continue;
                }
                return Err(e.context("LLM request failed"));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("LLM request failed")))
}

fn is_retryable(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}").to_ascii_lowercase();
    ["timed out", "timeout", "connection", "429", "500", "502", "503", "504", "overloaded"]
        .iter()
        .any(|needle| text.contains(needle))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

const SYSTEM_PROMPT: &str = "\
You reverse-engineer the layout of messy data files. You are given a sample \
of a file (never the whole file) and must respond with a single JSON object: \
a parsing spec that turns the file into one tidy table. The spec's JSON \
schema is authoritative; emit nothing outside it — no prose, no markdown \
fences.

Principles:
- The pipeline runs extraction -> transforms (in order) -> columns.
- All transforms operate on raw strings; typing happens last via `columns`.
- `columns` is a projection: only listed columns appear in the output. Use \
`source` for the post-transform column name and `name` for a clean \
snake_case output name. A `source` that does not exist after the transforms \
is the most common mistake — trace the header through each transform.
- Title blocks and footer/total rows: skip_rows. Multi-row headers: \
promote_header with rows > 1 (blank cells in the upper header rows inherit \
from the left, then rows are joined). Vertically merged / sparse category \
columns: fill_down. Repeated subtotal lines inside the body: \
drop_rows_matching. Wide month/quarter layouts: unpivot.
- Locale-specific tokens the date parser cannot handle are fixed with literal \
`replace` pairs in the column's `parse`. chrono understands English month \
names only, so a German column needs ALL the abbreviations that differ, not \
just the one you happened to see in the sample: \"Mär\"->\"Mar\", \
\"Mai\"->\"May\", \"Okt\"->\"Oct\", \"Dez\"->\"Dec\" (Jan, Feb, Apr, Jun, Jul, \
Aug, Sep and Nov are already English). French: \"Fév\"->\"Feb\", \
\"Avr\"->\"Apr\", \"Mai\"->\"May\", \"Juin\"->\"Jun\", \"Juil\"->\"Jul\", \
\"Aoû\"->\"Aug\", \"Déc\"->\"Dec\". The sample shows you only part of the \
file: cover every month, not only the visible ones.
- Separators are checked, not assumed: `thousands_separator` must group the \
integer part in threes, so `1,5` with thousands_separator=\",\" is an error, \
not 15. If a comma is the decimal point, set decimal_separator. Currency \
prefixes and units are removed with `strip` (a regex). Use decimal(p,s) for \
money; precision 38 is safe.
- `%Y` needs a four-digit year in the data; use `%y` for two-digit years.
- A timestamp `timezone` must be a fixed offset (\"UTC\", \"+02:00\"); the \
values are converted from it to UTC.
- fixed_width offsets count characters, not bytes.
- Set `confidence` honestly and put real caveats in `notes`.";

fn build_user_prompt(
    sample: &FileSample,
    draft: Option<&SniffResult>,
    hint: Option<&str>,
    feedback: Option<&Feedback>,
    schema_text: &str,
) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "File: {} ({} bytes{}{})\n",
        sample.file_name,
        sample.bytes,
        sample
            .encoding
            .as_deref()
            .map(|e| format!(", encoding {e}"))
            .unwrap_or_default(),
        if sample.sheets.is_empty() {
            String::new()
        } else {
            format!(", sheets {:?}", sample.sheets)
        }
    ));
    if sample.partial {
        p.push_str("The sample below is an excerpt; the file is larger.\n");
    }
    if let Some(h) = hint {
        p.push_str(&format!("User hint: {h}\n"));
    }
    p.push_str("\n--- sample begin ---\n");
    p.push_str(&sample.body);
    p.push_str("\n--- sample end ---\n");
    if let Some(d) = draft {
        // The draft carries column names and NA tokens the sniffer found in
        // rows beyond the sample. `sample_bytes` is the user's statement about
        // how much of their file may leave the machine, so the draft is capped
        // too rather than being an unbounded second channel.
        p.push_str(&format!(
            "\nA heuristic first pass produced this draft spec (confidence {:.2}). \
             Correct it where it is wrong; keep what is right:\n{}\n",
            d.confidence,
            truncate(
                &serde_json::to_string_pretty(&d.spec).unwrap_or_default(),
                MAX_DRAFT_CHARS
            )
        ));
    }
    if let Some(f) = feedback {
        p.push_str(&format!(
            "\nYour previous attempt was:\n{}\n\nIt failed. Fix exactly this problem \
             and emit the corrected spec:\n{}\n",
            truncate(&f.previous, MAX_FEEDBACK_CHARS),
            f.problem
        ));
    }
    // The schema also goes in `response_format`, but only some providers
    // enforce it — OpenAI's strict mode rejects a schema this shape, and a
    // non-strict one is advisory. A model that has never seen the contract
    // invents fields (`locale`) or omits required ones (`pattern`), and
    // `deny_unknown_fields` then rejects it. So it is stated outright.
    if !schema_text.is_empty() {
        p.push_str(
            "\nThe object you emit MUST validate against this JSON Schema. Every field \
             name is closed: emit no key that does not appear in it.\n",
        );
        p.push_str(schema_text);
        p.push('\n');
    }
    p.push_str("\nRespond with the JSON spec only.");
    p
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    t
}

// ---------------------------------------------------------------------------
// OpenAI-compatible backend (llama.cpp server, Ollama, vLLM, ...)
// ---------------------------------------------------------------------------

struct OpenAiCompatible {
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Provider-specific headers (OpenRouter's attribution pair). Never
    /// carries file content.
    extra_headers: Vec<(&'static str, String)>,
    client: reqwest::Client,
}

#[async_trait]
impl SpecInferencer for OpenAiCompatible {
    async fn complete(
        &self,
        system: &str,
        user: &str,
        schema: &serde_json::Value,
    ) -> Result<String> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let base_body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
        });

        // Constrained decoding, weakening only as far as each server forces.
        // `strict` is deliberately false: the schema uses $ref/definitions,
        // which strict validators reject outright — and losing the schema
        // entirely is a much bigger loss than losing strictness.
        let ladder = [
            Some(serde_json::json!({
                "type": "json_schema",
                "json_schema": {"name": "parse_spec", "schema": schema, "strict": false}
            })),
            Some(serde_json::json!({"type": "json_object"})),
            None,
        ];

        let mut last_err: Option<anyhow::Error> = None;
        for rf in ladder {
            let mut body = base_body.clone();
            if let Some(rf) = rf {
                body["response_format"] = rf;
            }
            match self.post(&url, &body).await {
                Ok(resp) => {
                    return resp
                        .pointer("/choices/0/message/content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            anyhow!("no content in completion response: {}", truncate(&resp.to_string(), 500))
                        });
                }
                Err(e) => {
                    // A transport failure will not be fixed by weakening the
                    // schema, so stop and let the transport retry handle it.
                    if is_retryable(&e) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("no response from {}", self.base_url)))
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }
}

impl OpenAiCompatible {
    async fn post(&self, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let mut req = self.client.post(url).json(body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        for (name, value) in &self.extra_headers {
            req = req.header(*name, value);
        }
        let resp = req.send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().await.context("reading response body")?;
        if !status.is_success() {
            bail!("{url} returned {status}: {}", truncate(&text, 500));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("{url} returned non-JSON: {}", truncate(&text, 300)))
    }
}

// ---------------------------------------------------------------------------
// Anthropic backend
// ---------------------------------------------------------------------------

struct Anthropic {
    model: String,
    api_key: String,
    client: reqwest::Client,
}

#[async_trait]
impl SpecInferencer for Anthropic {
    async fn complete(
        &self,
        system: &str,
        user: &str,
        schema: &serde_json::Value,
    ) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            // A parsing spec for a wide sheet is a few thousand tokens; 4096
            // used to truncate them into an unparseable tool input.
            "max_tokens": 16000,
            "system": system,
            "messages": [{"role": "user", "content": user}],
            "tools": [{
                "name": "emit_parse_spec",
                "description": "Emit the parsing spec for the sampled file.",
                "input_schema": schema
            }],
            "tool_choice": {"type": "tool", "name": "emit_parse_spec"}
        });
        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("POST api.anthropic.com/v1/messages")?;
        let status = resp.status();
        let text = resp.text().await.context("reading response body")?;
        if !status.is_success() {
            bail!("Anthropic API returned {status}: {}", truncate(&text, 500));
        }
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("non-JSON response: {}", truncate(&text, 300)))?;

        // A response can succeed at the HTTP level and still not contain an
        // answer. Checking why beats reporting "no tool_use block".
        match value.pointer("/stop_reason").and_then(|v| v.as_str()) {
            Some("max_tokens") => bail!(
                "the model hit its output limit before finishing the spec; the file's \
                 layout may be too wide to describe in one response"
            ),
            Some("refusal") => {
                let category = value
                    .pointer("/stop_details/category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unspecified");
                bail!("the model declined this request (category: {category})")
            }
            _ => {}
        }

        let content = value
            .pointer("/content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow!("no content array in response"))?;
        for block in content {
            if block.pointer("/type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(input) = block.pointer("/input") {
                    return Ok(input.to_string());
                }
            }
        }
        bail!("model response contained no tool_use block")
    }

    fn model_name(&self) -> String {
        self.model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("  {\"a\":1}  "), "{\"a\":1}");
        assert_eq!(strip_fences("```\n{}\n```"), "{}");
    }

    #[test]
    fn transport_errors_are_recognised() {
        assert!(is_retryable(&anyhow!("operation timed out")));
        assert!(is_retryable(&anyhow!("server returned 503: overloaded")));
        assert!(!is_retryable(&anyhow!("your JSON did not deserialize")));
    }

    #[test]
    fn feedback_is_capped() {
        let long = "x".repeat(10_000);
        let t = truncate(&long, 100);
        assert!(t.chars().count() < 200);
        assert!(t.contains("truncated"));
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn no_backend_configured_explains_the_options() {
        let cfg = Config::default();
        let msg = match make_inferencer(&cfg) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("backend none must not build an inferencer"),
        };
        assert!(msg.contains("tdy sniff"));
        assert!(msg.contains("local"));
    }

    #[test]
    fn the_prompt_carries_the_sample_and_the_draft() {
        let sample = FileSample {
            file_name: "x.csv".into(),
            bytes: 10,
            format: crate::sample::FormatGuess::Delimited,
            encoding: Some("utf-8".into()),
            ascii_only: true,
            body: "a,b\n1,2\n".into(),
            sheets: vec![],
            sampled_bytes: 8,
            partial: true,
        };
        let schema = serde_json::to_string(&ParseSpec::json_schema()).unwrap();
        let p = build_user_prompt(&sample, None, Some("a hint"), None, &schema);
        assert!(p.contains("a,b"));
        assert!(p.contains("a hint"));
        assert!(p.contains("excerpt"));
        // The contract must be in the prompt, not only in response_format:
        // most providers do not enforce the latter.
        assert!(p.contains("promote_header"), "the schema itself must be in the prompt");
        assert!(p.contains("MUST validate"));
    }
}
