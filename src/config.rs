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
}

impl Backend {
    fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Ok(Backend::None),
            "local" | "openai" | "openai-compatible" | "ollama" | "llama.cpp" => Ok(Backend::Local),
            "anthropic" | "claude" => Ok(Backend::Anthropic),
            other => anyhow::bail!(
                "unknown backend `{other}` (expected: none | local | anthropic)"
            ),
        }
    }

    /// True when using this backend sends file content off this machine.
    pub fn is_remote(self) -> bool {
        matches!(self, Backend::Anthropic)
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::None => "none",
            Backend::Local => "local",
            Backend::Anthropic => "anthropic",
        }
    }
}

/// The Anthropic model used when none is configured. Kept current
/// deliberately: an inference tier is only as good as the model behind it.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5";

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
    /// Refuse to parse a source file larger than this.
    pub max_file_bytes: u64,
    /// Refuse a table with more than this many cells (rows x columns).
    pub max_cells: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 4 * 1024 * 1024 * 1024, // 4 GiB
            max_cells: 400_000_000,
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
}

impl EnvVars {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        EnvVars {
            backend: get("TDY_BACKEND"),
            base_url: get("TDY_BASE_URL"),
            model: get("TDY_MODEL"),
            api_key_env: get("TDY_API_KEY_ENV"),
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
    if let Some(v) = &env.base_url {
        cfg.base_url = v.clone();
    }
    if let Some(v) = &overrides.base_url {
        cfg.base_url = v.clone();
    }
    let mut key_env_overridden = false;
    if let Some(v) = &env.api_key_env {
        cfg.api_key_env = v.clone();
        key_env_overridden = true;
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
    }

    // Backend-specific defaults.
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
    if cfg.limits.max_file_bytes == 0 || cfg.limits.max_cells == 0 {
        anyhow::bail!("limits must be greater than zero");
    }
    if cfg.backend == Backend::Local && !cfg.base_url.starts_with("http") {
        anyhow::bail!(
            "base_url must be an http(s) URL (got {:?}) in {where_}",
            cfg.base_url
        );
    }
    Ok(())
}

pub const SAMPLE_CONFIG: &str = r#"# ~/.config/tdy/config.toml
[inference]
# none      = heuristics only (default; nothing leaves the machine)
# local     = any OpenAI-compatible server: llama.cpp, Ollama, vLLM
# anthropic = Anthropic API (key read from the env var in api_key_env)
backend = "local"
base_url = "http://localhost:11434"   # llama.cpp server: http://localhost:8080
model = "qwen2.5-coder:32b"
# api_key_env = "ANTHROPIC_API_KEY"
confidence_threshold = 0.8
max_retries = 2
sample_bytes = 16384
timeout_seconds = 120

[limits]
# Guard rails, not policy: a file past these fails with an explanation
# instead of with the OOM killer.
max_file_bytes = 4294967296   # 4 GiB
max_cells = 400000000
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
        assert!(!c.backend.is_remote());
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
        assert!(c.backend.is_remote());
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
