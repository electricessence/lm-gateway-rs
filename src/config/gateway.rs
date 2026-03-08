//! Gateway and backend configuration types.

use serde::{Deserialize, Serialize};

use super::Provider;

/// Core gateway settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Port for the agent-facing client API (default: 8080).
    #[serde(default = "defaults::client_port")]
    pub client_port: u16,

    /// Port for the admin API + web UI (default: 8081).
    #[serde(default = "defaults::admin_port")]
    pub admin_port: u16,

    /// Number of recent requests to keep in the in-memory traffic log (default: 500).
    #[serde(default = "defaults::traffic_log_capacity")]
    pub traffic_log_capacity: usize,

    /// Log level override (also controlled by `RUST_LOG` env var).
    #[serde(default)]
    pub log_level: Option<String>,

    /// Maximum requests per minute per client IP on the client port.
    ///
    /// Leave unset (or set to 0) to disable rate limiting.
    /// The burst allowance equals half of this value, rounded up,
    /// so `rate_limit_rpm = 60` allows 60 req/min sustained and up to
    /// 30 back-to-back requests before the bucket empties.
    #[serde(default)]
    pub rate_limit_rpm: Option<u32>,

    /// Environment variable whose value is the Bearer token required for all
    /// admin API requests. Leave unset to disable admin authentication (only
    /// recommended when the admin port is strictly firewalled).
    ///
    /// Example: `admin_token_env = "LMG_ADMIN_TOKEN"`.
    #[serde(default)]
    pub admin_token_env: Option<String>,

    /// Profile used for requests that carry no (or an unrecognised) Bearer token.
    ///
    /// When `[[clients]]` entries are configured and a request arrives without a
    /// valid key, the gateway falls through to this profile instead of
    /// returning 401. Set to a restricted profile (e.g. `"default"`) for
    /// local/LAN access without requiring a key.
    ///
    /// When unset, unauthenticated requests are rejected with 401 whenever
    /// client keys are configured.
    ///
    /// Example: `public_profile = "default"`.
    #[serde(default)]
    pub public_profile: Option<String>,

    /// Number of additional attempts after the first failure (default: 0 = no retry).
    ///
    /// On each retry the gateway waits `retry_delay_ms` (doubled per attempt,
    /// capped at 2 s) before calling the backend again. Only transient errors
    /// (network failures, 5xx) benefit from retries; 4xx errors are not retried.
    #[serde(default)]
    pub max_retries: Option<u32>,

    /// Initial delay between retry attempts in milliseconds (default: 200).
    ///
    /// Doubles on each subsequent attempt, capped at 2000 ms.
    /// Ignored when `max_retries` is 0 or unset.
    #[serde(default)]
    pub retry_delay_ms: Option<u64>,

    /// Sliding-window size for backend health tracking (default: 10).
    ///
    /// The gateway tracks the last `health_window` requests per backend. In
    /// escalate mode, if a backend's error rate over this window exceeds
    /// `health_error_threshold`, that backend is skipped and the next tier is
    /// tried instead. Set to 0 to disable health-based routing entirely.
    #[serde(default)]
    pub health_window: Option<usize>,

    /// Hard timeout in milliseconds applied at the gateway level to the entire
    /// dispatch attempt (all retries included). Default: 120 000 (2 minutes).
    ///
    /// When a client disconnects before the backend responds, lm-gateway would
    /// otherwise hold the backend connection open until the backend's own
    /// `timeout_ms` fires — tying up the priority gate the whole time and
    /// blocking all lower-priority requests behind the ghost request.
    ///
    /// Setting this shorter than the backend's `timeout_ms` ensures the gate
    /// permit is released promptly and the backend TCP connection is torn down.
    ///
    /// Set to `0` to disable the gateway-level timeout entirely (not recommended).
    #[serde(default = "defaults::request_timeout_ms")]
    pub request_timeout_ms: Option<u64>,

    /// Error-rate threshold above which a backend is considered unhealthy
    /// (default: 0.7 = 70 %).
    ///
    /// Value in `(0.0, 1.0]`. A backend must have at least 3 samples in the
    /// window before it can be flagged as unhealthy. Set to `1.0` to
    /// effectively disable health-based skipping.
    #[serde(default)]
    pub health_error_threshold: Option<f64>,

    /// Directory containing per-profile TOML files.
    ///
    /// Each `*.toml` file in this directory is loaded as a `ProfileConfig`
    /// whose name is the file stem (e.g. `ha-auto.toml` → profile `ha-auto`).
    /// Directory profiles are merged **after** inline `[profiles.*]` sections
    /// and `conf.d/` overlays, so they take precedence on name collision.
    ///
    /// Relative paths are resolved against the parent of the main config file.
    /// Defaults to `profiles/` next to the config file when unset.
    ///
    /// ```toml
    /// [gateway]
    /// profile_dir = "profiles"
    /// ```
    #[serde(default)]
    pub profile_dir: Option<String>,
}

/// A reference to a secret value from one of the supported secret stores.
///
/// Use alongside (or instead of) `api_key_env` in [`BackendConfig`].
/// When both `api_key_secret` and `api_key_env` are present, `api_key_secret`
/// takes precedence.
///
/// ```toml
/// # Environment variable (backcompat shorthand):
/// api_key_env = "ANTHROPIC_KEY"
///
/// # Typed env-var form  (equivalent):
/// api_key_secret = { source = "env", var = "ANTHROPIC_KEY" }
///
/// # Docker / Kubernetes file secret:
/// api_key_secret = { source = "file", path = "/run/secrets/anthropic_key" }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SecretSource {
    /// Read the secret value from an environment variable.
    Env {
        /// Name of the environment variable.
        var: String,
    },
    /// Read the secret value from a file.
    ///
    /// Typical uses: Docker secrets (`/run/secrets/<name>`), Kubernetes
    /// mounted secrets, or any file-based secret store.
    /// Trailing newlines and carriage returns are stripped automatically.
    File {
        /// Absolute path to the file containing the secret.
        path: String,
    },
}

impl SecretSource {
    /// Resolve and return the secret, or `None` if unavailable.
    pub fn resolve(&self) -> Option<String> {
        match self {
            Self::Env { var } => std::env::var(var).ok().filter(|v| !v.is_empty()),
            Self::File { path } => std::fs::read_to_string(path)
                .ok()
                .map(|s| s.trim_end_matches(['\n', '\r']).to_owned())
                .filter(|v| !v.is_empty()),
        }
    }
}

/// A named backend (Ollama instance, OpenRouter, Anthropic direct, etc.).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BackendConfig {
    /// Base URL — must end without a trailing `/v1` (added by the client).
    pub base_url: String,

    /// Shorthand for `api_key_secret = { source = "env", var = "..." }`.
    ///
    /// Leave unset for keyless local backends (e.g., Ollama with no auth).
    /// When `api_key_secret` is also set, this field is ignored.
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Typed secret reference for the API key.
    ///
    /// Supports `"env"` (same as `api_key_env`) and `"file"` (Docker / k8s
    /// secrets and any file-based store). Takes precedence over `api_key_env`.
    #[serde(default)]
    pub api_key_secret: Option<SecretSource>,

    /// Request timeout in milliseconds (default: 30 000).
    #[serde(default = "defaults::timeout_ms")]
    pub timeout_ms: u64,

    /// Protocol adapter to use when talking to this backend.
    ///
    /// Defaults to [`Provider::OpenAI`] (passthrough). Set to `"anthropic"`
    /// for direct Anthropic API access, `"ollama"` for local Ollama, or
    /// `"open_router"` to enable OpenRouter-specific headers.
    #[serde(default)]
    pub provider: Provider,

    /// Default options merged into every request sent to an Ollama backend.
    /// Requested values in the incoming body always take precedence — these are
    /// only applied when absent. Only used when `provider = "ollama"`.
    ///
    /// Most useful for controlling the context window:
    /// ```toml
    /// [backends.ollama]
    /// default_options = { num_ctx = 16384 }
    /// ```
    #[serde(default)]
    pub default_options: Option<serde_json::Value>,
}

impl BackendConfig {
    /// Resolve the API key using the configured secret source.
    ///
    /// Checks `api_key_secret` first; falls back to `api_key_env`.
    /// Returns `None` if neither is configured or the value is unavailable.
    pub fn api_key(&self) -> Option<String> {
        if let Some(source) = &self.api_key_secret {
            return source.resolve();
        }
        self.api_key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
    }

    /// Returns `true` if this backend has any API key source configured
    /// (whether or not the value is currently resolvable).
    pub fn has_api_key_configured(&self) -> bool {
        self.api_key_secret.is_some() || self.api_key_env.is_some()
    }

    /// Returns the source type string (`"env"` or `"file"`) when a key is
    /// configured, or `None` for keyless backends.
    pub fn api_key_source_type(&self) -> Option<&'static str> {
        match &self.api_key_secret {
            Some(SecretSource::Env { .. }) => Some("env"),
            Some(SecretSource::File { .. }) => Some("file"),
            None if self.api_key_env.is_some() => Some("env"),
            None => None,
        }
    }
}

/// Default values for serde field defaults in this module.
pub(super) mod defaults {
    pub fn client_port() -> u16 { 8080 }
    pub fn admin_port() -> u16 { 8081 }
    pub fn traffic_log_capacity() -> usize { 500 }
    pub fn timeout_ms() -> u64 { 30_000 }
    pub fn request_timeout_ms() -> Option<u64> { Some(120_000) }
    pub fn classifier_timeout_ms() -> u64 { 10_000 }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn default_options_parses_from_toml() {
        let toml = r#"
[gateway]
client_port = 8080
admin_port  = 8081

[backends.ollama]
provider    = "ollama"
base_url    = "http://127.0.0.1:11434"
timeout_ms  = 300000
default_options = { num_ctx = 16384 }
"#;
        let config: Config = toml::from_str(toml).expect("TOML parse failed");
        let ollama = config.backends.get("ollama").expect("ollama backend missing");
        let opts = ollama.default_options.as_ref().expect("default_options is None");
        let map = opts.as_object().expect("default_options is not an object");
        let num_ctx = map.get("num_ctx").expect("num_ctx missing");
        assert_eq!(num_ctx.as_i64(), Some(16384), "num_ctx should be 16384, got: {num_ctx:?}");
    }
}
