//! Configuration for the inference tier and the safety limits.
//!
//! Resolution order (later wins): built-in defaults -> ~/.config/tdy/config.toml
//! -> TDY_* environment variables -> CLI flags.
//!
//! `backend = "none"` is the shipped default: heuristics-only, hard error
//! with guidance when they aren't confident. Nothing ever leaves the machine
//! unless someone explicitly configures a backend — and `"local"` (any
//! OpenAI-compatible server: llama.cpp, Ollama, vLLM) keeps it on-box even
//! then.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    None,
    /// Any OpenAI-compatible endpoint (llama.cpp server, Ollama, vLLM, ...).
    Local,
    Anthropic,
    /// OpenRouter: one OpenAI-compatible endpoint in front of many models.
    /// Wire-identical to `Local`, but hosted — so it is treated as remote
    /// for the purposes of telling you that your file is leaving.
    OpenRouter,
}

impl Backend {
    fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Ok(Backend::None),
            "local" | "openai" | "openai-compatible" | "ollama" | "llama.cpp" => Ok(Backend::Local),
            "anthropic" | "claude" => Ok(Backend::Anthropic),
            "openrouter" | "open-router" => Ok(Backend::OpenRouter),
            other => anyhow::bail!(
                "unknown backend `{other}` (expected: none | local | anthropic | openrouter)"
            ),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::None => "none",
            Backend::Local => "local",
            Backend::Anthropic => "anthropic",
            Backend::OpenRouter => "openrouter",
        }
    }
}

/// Is this base URL a server on this machine?
///
/// `backend = "local"` is a promise about *where the server is*, and pointing
/// it at a hosted endpoint is exactly how a file leaves the machine by
/// accident. So the promise is checked rather than taken on trust.
pub fn is_loopback_url(url: &str) -> bool {
    let rest = url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or_else(|| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host == "0.0.0.0"
        || host.starts_with("127.")
        || host.ends_with(".local")
        || host.ends_with(".localhost")
}

/// The Anthropic model used when none is configured. Kept current
/// deliberately: an inference tier is only as good as the model behind it.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5";

/// OpenRouter's default base URL. tdy appends `/v1/chat/completions`.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";

#[derive(Debug, Clone)]
pub struct Config {
    pub backend: Backend,
    /// Base URL for the local/OpenAI-compatible backend.
    pub base_url: String,
    pub model: String,
    /// Name of the env var holding the API key (never the key itself).
    pub api_key_env: String,
    /// Heuristic specs below this confidence escalate to the LLM tier
    /// (when a backend is configured).
    pub confidence_threshold: f32,
    pub max_retries: u32,
    /// Max bytes of the file shown to the model.
    pub sample_bytes: usize,
    /// Per-request HTTP timeout for the inference backends.
    pub http_timeout: Duration,
    pub limits: Limits,
}

/// Guard rails. These exist so that a pathological file fails with a sentence
/// you can act on instead of with the OOM killer.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Refuse to parse a source file larger than this. For a zip-based
    /// spreadsheet this is applied to the *uncompressed* total, which is
    /// what has to be held, not to the size on disk.
    pub max_file_bytes: u64,
    /// Refuse a table with more than this many cells (rows x columns) —
    /// checked against what a spreadsheet *declares*, before its grid is
    /// allocated, as well as against the table once built.
    ///
    /// This is the bound for work that is *materialised*: spreadsheets, JSON,
    /// and any spec the streaming executor declines.
    pub max_cells: u64,
    /// A bound on *work* for the streaming executor, not on memory.
    ///
    /// Streaming memory does not depend on how many cells a file has — the
    /// rows are never accumulated and a batch is bounded by cells of its own —
    /// so this exists only to stop a run that would take unreasonably long.
    /// `max_file_bytes` is the bound that usually bites first.
    pub max_streamed_cells: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 4 * 1024 * 1024 * 1024, // 4 GiB
            // Both numbers are measured, and both stand for about the same
            // ceiling — roughly 6 GB — on paths whose per-cell cost differs
            // by a factor of seven. One knob could not do that honestly.
            //
            // Materialised, a cell costs ~122 bytes: calamine's Data, then
            // our own String, then the Arrow array. 50M of them is ~6.1 GB.
            // (This was 400M until a 898-byte .ods was measured at 4.8 GB —
            // 400M stood for ~48 GB, which is not a guard rail, it is a
            // number larger than the machine.)
            max_cells: 50_000_000,
            // Streaming holds neither the rows nor the decoded text, and its
            // batches are bounded by cells, so its memory does not depend on
            // this at all. It is a "this would run for hours" guard, and it is
            // set high enough that `max_file_bytes` is what normally stops a
            // run: 4 GiB of an 8-column CSV is only ~690M cells.
            //
            // It was 200M briefly, calibrated against a memory cost that the
            // streaming executor no longer has — which refused a 1.25 GB file
            // that in fact reads in 88 MB.
            max_streamed_cells: 2_000_000_000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: Backend::None,
            base_url: "http://localhost:11434".into(), // Ollama's default; llama.cpp: 8080
            model: String::new(),
            api_key_env: String::new(),
            confidence_threshold: 0.8,
            max_retries: 2,
            sample_bytes: 16 * 1024,
            http_timeout: Duration::from_secs(120),
            limits: Limits::default(),
        }
    }
}

impl Config {
    /// True when using this configuration sends file content off this
    /// machine — the question the user actually cares about, which for
    /// `local` depends on where it is pointed.
    pub fn is_remote(&self) -> bool {
        match self.backend {
            Backend::None => false,
            Backend::Anthropic | Backend::OpenRouter => true,
            Backend::Local => !is_loopback_url(&self.base_url),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    inference: FileInference,
    #[serde(default)]
    limits: FileLimits,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInference {
    backend: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api_key_env: Option<String>,
    confidence_threshold: Option<f32>,
    max_retries: Option<u32>,
    sample_bytes: Option<usize>,
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLimits {
    max_file_bytes: Option<u64>,
    max_cells: Option<u64>,
    max_streamed_cells: Option<u64>,
}

/// CLI-level overrides collected by clap.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

pub fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tdy").join("config.toml"))
}

pub fn load(overrides: &Overrides) -> Result<Config> {
    let path = config_file_path();
    let text = match &path {
        Some(p) if p.exists() => Some(
            std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?,
        ),
        _ => None,
    };
    let shown = path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/tdy/config.toml".into());
    resolve(text.as_deref(), &EnvVars::from_process(), overrides, &shown)
}

/// The environment, captured so the resolution logic can be tested without
/// mutating process state.
#[derive(Debug, Default, Clone)]
pub struct EnvVars {
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    /// How many times the model may be asked to fix its own spec. Worth
    /// raising for a hard file: each retry carries the exact error back, so
    /// the loop genuinely converges — it just costs another request.
    pub max_retries: Option<String>,
}

impl EnvVars {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        EnvVars {
            backend: get("TDY_BACKEND"),
            base_url: get("TDY_BASE_URL"),
            model: get("TDY_MODEL"),
            api_key_env: get("TDY_API_KEY_ENV"),
            max_retries: get("TDY_MAX_RETRIES"),
        }
    }
}

/// Pure resolution, so the precedence rules are testable.
pub fn resolve(
    file_text: Option<&str>,
    env: &EnvVars,
    overrides: &Overrides,
    config_path_for_messages: &str,
) -> Result<Config> {
    let mut cfg = Config::default();

    // 1. Config file.
    let mut file_backend: Option<Backend> = None;
    let mut model_from_file = false;
    let mut key_env_from_file = false;
    let mut base_url_from_file = false;
    if let Some(text) = file_text {
        let fc: FileConfig = toml::from_str(text)
            .with_context(|| format!("invalid config {config_path_for_messages}"))?;
        let i = fc.inference;
        if let Some(b) = i.backend {
            cfg.backend = Backend::parse(&b)?;
            file_backend = Some(cfg.backend);
        }
        if let Some(v) = i.base_url {
            cfg.base_url = v;
            base_url_from_file = true;
        }
        if let Some(v) = i.model {
            cfg.model = v;
            model_from_file = true;
        }
        if let Some(v) = i.api_key_env {
            cfg.api_key_env = v;
            key_env_from_file = true;
        }
        if let Some(v) = i.confidence_threshold {
            cfg.confidence_threshold = v;
        }
        if let Some(v) = i.max_retries {
            cfg.max_retries = v;
        }
        if let Some(v) = i.sample_bytes {
            cfg.sample_bytes = v;
        }
        if let Some(v) = i.timeout_seconds {
            cfg.http_timeout = Duration::from_secs(v);
        }
        if let Some(v) = fc.limits.max_file_bytes {
            cfg.limits.max_file_bytes = v;
        }
        if let Some(v) = fc.limits.max_streamed_cells {
            cfg.limits.max_streamed_cells = v;
        }
        if let Some(v) = fc.limits.max_cells {
            cfg.limits.max_cells = v;
        }
    }

    // 2. Environment, 3. CLI flags.
    let mut model_overridden = false;
    if let Some(v) = &env.model {
        cfg.model = v.clone();
        model_overridden = true;
    }
    if let Some(v) = &overrides.model {
        cfg.model = v.clone();
        model_overridden = true;
    }
    let mut base_url_overridden = false;
    if let Some(v) = &env.base_url {
        cfg.base_url = v.clone();
        base_url_overridden = true;
    }
    if let Some(v) = &overrides.base_url {
        cfg.base_url = v.clone();
        base_url_overridden = true;
    }
    let mut key_env_overridden = false;
    if let Some(v) = &env.api_key_env {
        cfg.api_key_env = v.clone();
        key_env_overridden = true;
    }
    if let Some(v) = &env.max_retries {
        cfg.max_retries = v.trim().parse().with_context(|| {
            format!("TDY_MAX_RETRIES must be a whole number, got {v:?}")
        })?;
    }
    let mut backend_overridden = false;
    if let Some(b) = &env.backend {
        cfg.backend = Backend::parse(b)?;
        backend_overridden = true;
    }
    if let Some(b) = &overrides.backend {
        cfg.backend = Backend::parse(b)?;
        backend_overridden = true;
    }

    // Switching the backend on the command line must not silently carry the
    // *other* backend's model name or key variable along with it: pointing
    // `--backend local` at a config written for Anthropic would otherwise ask
    // a llama.cpp server for "claude-opus-5".
    if backend_overridden && Some(cfg.backend) != file_backend {
        if model_from_file && !model_overridden {
            cfg.model.clear();
        }
        if key_env_from_file && !key_env_overridden {
            cfg.api_key_env.clear();
        }
        if base_url_from_file && !base_url_overridden {
            cfg.base_url = Config::default().base_url;
            base_url_from_file = false;
        }
    }

    // Backend-specific defaults.
    if cfg.backend == Backend::OpenRouter {
        if cfg.api_key_env.is_empty() {
            cfg.api_key_env = "OPENROUTER_API_KEY".into();
        }
        if !base_url_overridden && !base_url_from_file {
            cfg.base_url = OPENROUTER_BASE_URL.into();
        }
        if cfg.model.is_empty() {
            anyhow::bail!(
                "backend `openrouter` needs a model name — it fronts hundreds of them, \
                 at very different prices. Set --model, TDY_MODEL, or [inference].model \
                 in {config_path_for_messages}; `openai/gpt-4o-mini` is a cheap place \
                 to start."
            );
        }
    }
    if cfg.backend == Backend::Anthropic {
        if cfg.api_key_env.is_empty() {
            cfg.api_key_env = "ANTHROPIC_API_KEY".into();
        }
        if cfg.model.is_empty() {
            cfg.model = DEFAULT_ANTHROPIC_MODEL.into();
        }
    }
    if cfg.backend == Backend::Local && cfg.model.is_empty() {
        anyhow::bail!(
            "backend `local` needs a model name: set --model, TDY_MODEL, or \
             [inference].model in {config_path_for_messages}"
        );
    }

    validate(&cfg, config_path_for_messages)?;
    Ok(cfg)
}

fn validate(cfg: &Config, where_: &str) -> Result<()> {
    if !(0.0..=1.0).contains(&cfg.confidence_threshold) || cfg.confidence_threshold.is_nan() {
        anyhow::bail!(
            "confidence_threshold must be between 0 and 1 (got {}) in {where_}",
            cfg.confidence_threshold
        );
    }
    if cfg.sample_bytes < 512 {
        anyhow::bail!(
            "sample_bytes must be at least 512 (got {}): a smaller sample cannot \
             show the model a usable part of the file",
            cfg.sample_bytes
        );
    }
    if cfg.sample_bytes > 8 * 1024 * 1024 {
        anyhow::bail!(
            "sample_bytes of {} would send several megabytes of your file to the \
             model; cap it at 8 MiB or lower",
            cfg.sample_bytes
        );
    }
    if cfg.max_retries > 20 {
        anyhow::bail!("max_retries of {} is unreasonable (max 20)", cfg.max_retries);
    }
    if cfg.http_timeout.as_secs() == 0 || cfg.http_timeout.as_secs() > 3600 {
        anyhow::bail!("timeout_seconds must be between 1 and 3600");
    }
    if cfg.limits.max_file_bytes == 0
        || cfg.limits.max_cells == 0
        || cfg.limits.max_streamed_cells == 0
    {
        anyhow::bail!("limits must be greater than zero");
    }
    if matches!(cfg.backend, Backend::Local | Backend::OpenRouter)
        && !cfg.base_url.starts_with("http")
    {
        anyhow::bail!(
            "base_url must be an http(s) URL (got {:?}) in {where_}",
            cfg.base_url
        );
    }
    Ok(())
}

pub const SAMPLE_CONFIG: &str = r#"# ~/.config/tdy/config.toml
[inference]
# none       = heuristics only (default; nothing leaves the machine)
# local      = any OpenAI-compatible server: llama.cpp, Ollama, vLLM
# anthropic  = Anthropic API      (key from the env var named in api_key_env)
# openrouter = OpenRouter         (key from OPENROUTER_API_KEY by default)
#
# `local` is checked, not trusted: pointed at a non-loopback base_url it is
# reported as remote, and tdy tells you how many bytes are leaving.
backend = "local"
base_url = "http://localhost:11434"   # llama.cpp server: http://localhost:8080
model = "qwen2.5-coder:32b"
# api_key_env = "ANTHROPIC_API_KEY"
# For OpenRouter, base_url defaults to https://openrouter.ai/api and the
# model is required, e.g. model = "openai/gpt-4o-mini"
confidence_threshold = 0.8
max_retries = 2
sample_bytes = 16384
timeout_seconds = 120

[limits]
# Guard rails, not policy: a file past these fails with an explanation
# instead of with the OOM killer. max_file_bytes applies to the uncompressed
# contents of a zip-based spreadsheet, not to its size on disk.
# max_cells bounds materialised work (spreadsheets, JSON, unusual specs) at
# ~122 bytes a cell, so 50M is a ceiling of roughly 6 GB. Raise it if you have
# the RAM and mean it.
# max_streamed_cells bounds *time*, not memory: streaming holds neither the
# rows nor the decoded text, so its cost does not follow the cell count.
# max_file_bytes is normally what stops a long run first.
max_file_bytes = 4294967296   # 4 GiB
max_cells = 50000000
max_streamed_cells = 2000000000
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn res(file: Option<&str>, env: EnvVars, ov: Overrides) -> Result<Config> {
        resolve(file, &env, &ov, "test-config")
    }

    #[test]
    fn defaults_are_offline() {
        let c = res(None, EnvVars::default(), Overrides::default()).unwrap();
        assert_eq!(c.backend, Backend::None);
        assert!(!c.is_remote());
    }

    #[test]
    fn anthropic_gets_a_current_model_by_default() {
        let c = res(
            Some("[inference]\nbackend = \"anthropic\"\n"),
            EnvVars::default(),
            Overrides::default(),
        )
        .unwrap();
        assert_eq!(c.model, DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(c.api_key_env, "ANTHROPIC_API_KEY");
        assert!(c.is_remote());
    }

    #[test]
    fn cli_overrides_env_overrides_file() {
        let file = "[inference]\nbackend = \"local\"\nmodel = \"from-file\"\n";
        let env = EnvVars { model: Some("from-env".into()), ..Default::default() };
        let c = res(Some(file), env.clone(), Overrides::default()).unwrap();
        assert_eq!(c.model, "from-env");
        let c = res(
            Some(file),
            env,
            Overrides { model: Some("from-cli".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(c.model, "from-cli");
    }

    #[test]
    fn switching_backend_drops_the_other_backends_model() {
        let file = "[inference]\nbackend = \"anthropic\"\nmodel = \"claude-opus-5\"\n";
        // Asking for a local server must not request "claude-opus-5" from it.
        let err = res(
            Some(file),
            EnvVars::default(),
            Overrides { backend: Some("local".into()), ..Default::default() },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("needs a model name"));

        // ...unless a model is given alongside the switch.
        let c = res(
            Some(file),
            EnvVars::default(),
            Overrides { backend: Some("local".into()), model: Some("qwen".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(c.model, "qwen");
    }

    #[test]
    fn same_backend_keeps_its_model() {
        let file = "[inference]\nbackend = \"local\"\nmodel = \"qwen\"\n";
        let c = res(
            Some(file),
            EnvVars::default(),
            Overrides { backend: Some("local".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(c.model, "qwen");
    }

    #[test]
    fn openrouter_defaults_to_its_endpoint_and_key() {
        let c = res(
            Some("[inference]\nbackend = \"openrouter\"\nmodel = \"openai/gpt-4o-mini\"\n"),
            EnvVars::default(),
            Overrides::default(),
        )
        .unwrap();
        assert_eq!(c.base_url, OPENROUTER_BASE_URL);
        assert_eq!(c.api_key_env, "OPENROUTER_API_KEY");
        assert!(c.is_remote());
    }

    #[test]
    fn openrouter_insists_on_a_model() {
        let err = res(
            Some("[inference]\nbackend = \"openrouter\"\n"),
            EnvVars::default(),
            Overrides::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("needs a model name"));
    }

    #[test]
    fn a_local_backend_pointed_at_the_internet_is_reported_as_remote() {
        // The whole value of `backend = "local"` is the promise that nothing
        // leaves; pointing it at a hosted endpoint must not keep the promise.
        let hosted = res(
            Some("[inference]\nbackend = \"local\"\nmodel = \"m\"\nbase_url = \"https://openrouter.ai/api\"\n"),
            EnvVars::default(),
            Overrides::default(),
        )
        .unwrap();
        assert!(hosted.is_remote());

        let on_box = res(
            Some("[inference]\nbackend = \"local\"\nmodel = \"m\"\n"),
            EnvVars::default(),
            Overrides::default(),
        )
        .unwrap();
        assert!(!on_box.is_remote());
    }

    #[test]
    fn loopback_urls_are_recognised() {
        for u in [
            "http://localhost:11434",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/v1",
            "http://box.local:1234",
        ] {
            assert!(is_loopback_url(u), "{u}");
        }
        for u in [
            "https://openrouter.ai/api",
            "https://api.example.com",
            "http://10.0.0.5:8080",
        ] {
            assert!(!is_loopback_url(u), "{u}");
        }
    }

    #[test]
    fn max_retries_can_be_raised_from_the_environment() {
        let c = res(
            None,
            EnvVars { max_retries: Some("5".into()), ..Default::default() },
            Overrides::default(),
        )
        .unwrap();
        assert_eq!(c.max_retries, 5);
        assert!(res(
            None,
            EnvVars { max_retries: Some("lots".into()), ..Default::default() },
            Overrides::default()
        )
        .is_err());
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for bad in [
            "[inference]\nconfidence_threshold = 1.5\n",
            "[inference]\nconfidence_threshold = -0.2\n",
            "[inference]\nsample_bytes = 10\n",
            "[inference]\nmax_retries = 500\n",
            "[inference]\ntimeout_seconds = 0\n",
            "[limits]\nmax_cells = 0\n",
        ] {
            assert!(res(Some(bad), EnvVars::default(), Overrides::default()).is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(res(
            Some("[inference]\nbakend = \"local\"\n"),
            EnvVars::default(),
            Overrides::default()
        )
        .is_err());
    }

    #[test]
    fn unknown_backend_name_is_rejected() {
        assert!(res(
            Some("[inference]\nbackend = \"gpt\"\n"),
            EnvVars::default(),
            Overrides::default()
        )
        .is_err());
    }

    #[test]
    fn the_sample_config_is_valid() {
        let c = res(Some(SAMPLE_CONFIG), EnvVars::default(), Overrides::default()).unwrap();
        assert_eq!(c.backend, Backend::Local);
        assert_eq!(c.http_timeout, Duration::from_secs(120));
        assert_eq!(c.limits.max_file_bytes, 4 * 1024 * 1024 * 1024);
    }
}
