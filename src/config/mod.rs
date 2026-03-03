//! Configuration types for lm-gateway.
//!
//! Config is loaded once at startup from a TOML file and validated before the
//! server opens any ports. Invalid configs are rejected with a clear error
//! rather than silently falling back to defaults.
//!
//! # Example
//! ```toml
//! [gateway]
//! client_port = 8080
//!
//! [backends.ollama]
//! base_url = "http://localhost:11434"
//!
//! [[tiers]]
//! name    = "local:fast"
//! backend = "ollama"
//! model   = "qwen2.5:1.5b"
//!
//! [aliases]
//! "hint:fast" = "local:fast"
//!
//! [profiles.default]
//! mode           = "dispatch"
//! classifier     = "local:fast"
//! max_auto_tier  = "local:fast"
//! ```

use std::{collections::HashMap, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

mod gateway;
mod profile;

// Re-exported for downstream code that matches on BackendConfig::api_key_secret.
#[allow(unused_imports)]
pub use gateway::{BackendConfig, GatewayConfig, SecretSource};
pub use profile::{DEFAULT_CLASSIFIER_PROMPT, ProfileConfig, RoutingMode, TierConfig};

/// Which API protocol a backend speaks.
///
/// lm-gateway normalises all inter-agent traffic to OpenAI's chat-completions
/// schema; each [`Provider`] variant maps to an adapter that handles any
/// necessary request/response translation at the edge.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Standard OpenAI `/v1/chat/completions` protocol.
    /// Also used by LM Studio, vLLM, LocalAI, and many others.
    #[default]
    OpenAI,
    /// OpenRouter — OpenAI-compatible wire format.
    /// Kept as a distinct variant so the router can inject the
    /// `HTTP-Referer` and `X-Title` headers that OpenRouter recommends.
    OpenRouter,
    /// Ollama local inference server. Uses Ollama's OpenAI-compat endpoint
    /// by default; future versions may use the native `/api/chat` path.
    Ollama,
    /// Anthropic Messages API (`/v1/messages`).
    /// Request and response shapes are translated to/from the OpenAI schema.
    Anthropic,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OpenAI => "openai",
            Self::OpenRouter => "openrouter",
            Self::Ollama => "ollama",
            Self::Anthropic => "anthropic",
        })
    }
}

/// A per-client API key binding.
///
/// The gateway reads the actual key value from the environment variable named
/// by `key_env` at startup. This keeps secrets out of the config file.
///
/// ```toml
/// [[clients]]
/// key_env = "CLIENT_ACME_KEY"
/// profile = "economy"
///
/// [[clients]]
/// key_env = "CLIENT_INTERNAL_KEY"
/// profile = "expert"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientConfig {
    /// Name of the environment variable whose value is this client's Bearer token.
    pub key_env: String,
    /// The profile to use when this client's key is matched.
    pub profile: String,
}

/// Top-level gateway configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub gateway: GatewayConfig,

    /// Named backends (Ollama, OpenRouter, Anthropic direct, etc.)
    #[serde(default)]
    pub backends: HashMap<String, BackendConfig>,

    /// Routing tiers — ordered ladder used for escalation.
    #[serde(default)]
    pub tiers: Vec<TierConfig>,

    /// Model/alias → tier name mappings.
    ///
    /// Clients send `model = "hint:fast"` — this maps it to the `local:fast` tier.
    #[serde(default)]
    pub aliases: HashMap<String, String>,

    /// Named routing profiles. The `default` profile is used when no client key is matched.
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,

    /// Per-client API key → profile mappings.
    ///
    /// Each entry binds a Bearer token (loaded from an env var at startup) to a
    /// named profile. When a client presents a key that matches an entry, that
    /// profile is used for the request. When no key is presented, or the key does
    /// not match any entry, the `default` profile is used (if configured).
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
}

/// Deep-merge `overlay` into `base` in-place.
///
/// - **Tables**: keys in `overlay` recursively override/extend `base`.
/// - **Arrays of tables that have a `name` field**: `overlay` entries replace
///   same-named `base` entries in-place (order preserved); new names append.
/// - **All other arrays and scalars**: `overlay` replaces `base` wholesale.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    use toml::Value;
    match (base, overlay) {
        (Value::Table(base_t), Value::Table(overlay_t)) => {
            for (key, ov_val) in overlay_t {
                match base_t.get_mut(&key) {
                    Some(base_val) => deep_merge(base_val, ov_val),
                    None => {
                        base_t.insert(key, ov_val);
                    }
                }
            }
        }
        (Value::Array(base_arr), Value::Array(overlay_arr)) => {
            for ov_item in overlay_arr {
                // Named-table deduplication: if the overlay item is a table
                // with a `name` key, replace the existing entry with the
                // same name rather than appending a duplicate.
                let maybe_name = if let Value::Table(ref t) = ov_item {
                    t.get("name").and_then(|v| v.as_str()).map(str::to_owned)
                } else {
                    None
                };
                if let Some(name) = maybe_name {
                    if let Some(existing) = base_arr.iter_mut().find(|v| {
                        v.as_table()
                            .and_then(|t| t.get("name"))
                            .and_then(|n| n.as_str())
                            == Some(&name)
                    }) {
                        *existing = ov_item;
                        continue;
                    }
                }
                base_arr.push(ov_item);
            }
        }
        // Scalar / mixed: overlay wins.
        (base, overlay) => *base = overlay,
    }
}

impl Config {
    /// Load configuration from `path`, then layer any `*.toml` files found in
    /// a `conf.d/` directory sitting next to `path` (alphabetically ordered).
    ///
    /// ## Merge rules
    ///
    /// | Section | Behaviour |
    /// |---|---|
    /// | `[gateway]`, `[backends.*]`, `[aliases]`, `[profiles.*]` | Key-level merge — overlay wins per key |
    /// | `[[tiers]]`, `[[clients]]` | Deduplicated by `name` field — overlay replaces same-named entry; new names append |
    ///
    /// A minimal `conf.d/local.toml` only needs to contain the sections it overrides:
    ///
    /// ```toml
    /// # conf.d/local.toml — machine-specific overrides, untracked in git
    /// [backends.ollama]
    /// base_url = "http://192.168.1.50:11434"
    ///
    /// [[tiers]]
    /// name    = "local:fast"
    /// backend = "ollama"
    /// model   = "qwen3:1.7b"
    /// ```
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut base: toml::Value = toml::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))?;

        // Layer conf.d/*.toml files alphabetically.
        let conf_d = path.parent().unwrap_or(Path::new(".")).join("conf.d");
        if conf_d.is_dir() {
            let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&conf_d)
                .with_context(|| format!("reading conf.d directory {}", conf_d.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
                .collect();
            entries.sort();

            for entry in entries {
                let overlay_content = std::fs::read_to_string(&entry)
                    .with_context(|| format!("reading {}", entry.display()))?;
                let overlay: toml::Value = toml::from_str(&overlay_content)
                    .with_context(|| format!("parsing {}", entry.display()))?;
                deep_merge(&mut base, overlay);
            }
        }

        // Serialize back to string and use toml::from_str exclusively (see gotchas.md).
        let merged = toml::to_string(&base).context("re-serializing merged config")?;
        let config: Self = toml::from_str(&merged).context("deserializing merged config")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        // Every tier must reference a known backend
        for tier in &self.tiers {
            anyhow::ensure!(
                self.backends.contains_key(&tier.backend),
                "tier `{}` references unknown backend `{}`",
                tier.name,
                tier.backend
            );
        }

        // Every alias must map to a known tier
        let tier_names: std::collections::HashSet<&str> =
            self.tiers.iter().map(|t| t.name.as_str()).collect();
        for (alias, tier) in &self.aliases {
            anyhow::ensure!(
                tier_names.contains(tier.as_str()),
                "alias `{}` maps to unknown tier `{}`",
                alias,
                tier
            );
        }

        // Every profile classifier must be a known tier
        for (name, profile) in &self.profiles {
            anyhow::ensure!(
                tier_names.contains(profile.classifier.as_str()),
                "profile `{}` classifier references unknown tier `{}`",
                name,
                profile.classifier
            );
        }

        // Every client entry must reference a known profile
        let profile_names: std::collections::HashSet<&str> =
            self.profiles.keys().map(|k| k.as_str()).collect();
        for client in &self.clients {
            anyhow::ensure!(
                profile_names.contains(client.profile.as_str()),
                "[[clients]] entry with key_env `{}` references unknown profile `{}`",
                client.key_env,
                client.profile
            );
        }

        Ok(())
    }

    /// Resolve a model string to a [`TierConfig`], following alias indirection.
    ///
    /// Lookup order:
    /// 1. Try `model` as an alias key → follow to tier name.
    /// 2. Try `model` as a direct tier name.
    /// 3. Return `None` if neither matches.
    pub fn resolve_tier<'a>(&'a self, model: &str) -> Option<&'a TierConfig> {
        let tier_name = self.aliases.get(model).map(|s| s.as_str()).unwrap_or(model);
        self.tiers.iter().find(|t| t.name == tier_name)
    }

    /// Return the named profile, falling back to `"default"`.
    ///
    /// Returns `None` only if neither the named profile nor a `"default"` profile exists.
    pub fn profile(&self, name: &str) -> Option<&ProfileConfig> {
        self.profiles.get(name).or_else(|| self.profiles.get("default"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn minimal_config() -> Config {
        toml::from_str(
            r#"
            [gateway]
            client_port = 8080
            admin_port  = 8081
            traffic_log_capacity = 500

            [backends.ollama]
            base_url = "http://localhost:11434"

            [[tiers]]
            name    = "local:fast"
            backend = "ollama"
            model   = "qwen2.5:1.5b"

            [[tiers]]
            name    = "cloud:economy"
            backend = "ollama"
            model   = "qwen2.5:7b"

            [aliases]
            "hint:fast"  = "local:fast"
            "hint:cloud" = "cloud:economy"

            [profiles.default]
            mode          = "dispatch"
            classifier    = "local:fast"
            max_auto_tier = "cloud:economy"
            "#,
        )
        .expect("minimal config should parse")
    }

    // -----------------------------------------------------------------------
    // Parsing & validation
    // -----------------------------------------------------------------------

    #[test]
    fn parse_example_config() {
        let content = include_str!("../../config.example.toml");
        let config: Config = toml::from_str(content).expect("example config should parse");
        config.validate().expect("example config should be valid");
    }

    #[test]
    fn validation_rejects_tier_with_unknown_backend() {
        let mut config = minimal_config();
        config.tiers.push(TierConfig {
            name: "bad:tier".into(),
            backend: "nonexistent".into(),
            model: "x".into(),
            think: None,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_alias_pointing_to_unknown_tier() {
        let mut config = minimal_config();
        config.aliases.insert("bad:alias".into(), "no-such-tier".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_profile_with_unknown_classifier() {
        let mut config = minimal_config();
        config.profiles.insert(
            "bad".into(),
            ProfileConfig {
                mode: RoutingMode::Dispatch,
                classifier: "no-such-tier".into(),
                max_auto_tier: "local:fast".into(),
                expert_requires_flag: false,
                rate_limit_rpm: None,
                classifier_prompt: None,
                system_prompt: None,
            },
        );
        assert!(config.validate().is_err());
    }

    // -----------------------------------------------------------------------
    // Tier resolution
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_tier_by_direct_name() {
        let config = minimal_config();
        let tier = config.resolve_tier("local:fast");
        assert!(tier.is_some());
        assert_eq!(tier.unwrap().name, "local:fast");
    }

    #[test]
    fn resolve_tier_via_alias() {
        let config = minimal_config();
        let tier = config.resolve_tier("hint:fast");
        assert!(tier.is_some());
        assert_eq!(tier.unwrap().name, "local:fast");
    }

    #[test]
    fn resolve_tier_returns_none_for_unknown() {
        let config = minimal_config();
        assert!(config.resolve_tier("completely:unknown").is_none());
    }

    // -----------------------------------------------------------------------
    // Profile lookup
    // -----------------------------------------------------------------------

    #[test]
    fn profile_returns_named_profile_when_present() {
        let config = minimal_config();
        assert!(config.profile("default").is_some());
    }

    #[test]
    fn profile_falls_back_to_default_for_unknown_name() {
        let config = minimal_config();
        // "nonexistent" doesn't exist, should fall back to "default"
        assert!(config.profile("nonexistent").is_some());
    }

    #[test]
    fn profile_returns_none_when_neither_named_nor_default_exists() {
        let mut config = minimal_config();
        config.profiles.clear();
        assert!(config.profile("anything").is_none());
    }

    // -----------------------------------------------------------------------
    // Routing mode deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn routing_mode_deserializes_from_snake_case() {
        // RoutingMode needs a containing table to parse via toml::from_str.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            mode: RoutingMode,
        }
        let dispatch: Wrapper = toml::from_str("mode = \"dispatch\"").unwrap();
        assert_eq!(dispatch.mode, RoutingMode::Dispatch);

        let escalate: Wrapper = toml::from_str("mode = \"escalate\"").unwrap();
        assert_eq!(escalate.mode, RoutingMode::Escalate);

        let classify: Wrapper = toml::from_str("mode = \"classify\"").unwrap();
        assert_eq!(classify.mode, RoutingMode::Classify);
    }

    #[test]
    fn gateway_defaults_are_applied_when_section_is_minimal() {
        // [gateway] must be present, but all its fields have defaults.
        // Verify that omitting optional fields (log_level, rate_limit_rpm, etc.)
        // leaves the required fields at their documented defaults.
        let config: Config = toml::from_str(
            r#"
            [gateway]
            client_port = 8080
            admin_port  = 8081
            traffic_log_capacity = 500

            [backends.x]
            base_url = "http://x"

            [[tiers]]
            name    = "t"
            backend = "x"
            model   = "m"

            [profiles.default]
            classifier    = "t"
            max_auto_tier = "t"
            "#,
        )
        .expect("should parse");
        assert_eq!(config.gateway.client_port, 8080);
        assert_eq!(config.gateway.admin_port, 8081);
        assert_eq!(config.gateway.traffic_log_capacity, 500);
        assert!(config.gateway.log_level.is_none());
        assert!(config.gateway.rate_limit_rpm.is_none());
        assert!(config.gateway.admin_token_env.is_none());
    }

    // -----------------------------------------------------------------------
    // Provider deserialization (lowercase)
    // -----------------------------------------------------------------------

    #[test]
    fn provider_deserializes_lowercase() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            provider: Provider,
        }
        let cases = [
            ("openai", Provider::OpenAI),
            ("openrouter", Provider::OpenRouter),
            ("ollama", Provider::Ollama),
            ("anthropic", Provider::Anthropic),
        ];
        for (s, expected) in cases {
            let w: Wrapper = toml::from_str(&format!("provider = \"{s}\"")).unwrap();
            assert_eq!(w.provider, expected, "failed for {s}");
        }
    }

    #[test]
    fn provider_display_matches_serde_key() {
        // Display and serde round-trip must use the same lowercase strings.
        let cases = [
            (Provider::OpenAI, "openai"),
            (Provider::OpenRouter, "openrouter"),
            (Provider::Ollama, "ollama"),
            (Provider::Anthropic, "anthropic"),
        ];
        for (variant, expected) in cases {
            assert_eq!(variant.to_string(), expected);
        }
    }

    // -----------------------------------------------------------------------
    // deep_merge helpers
    // -----------------------------------------------------------------------

    #[test]
    fn deep_merge_table_overlay_wins_per_key() {
        let mut base: toml::Value = toml::from_str(
            r#"
[gateway]
client_port = 8080
admin_port  = 8081
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[gateway]
client_port = 9090
"#,
        )
        .unwrap();
        deep_merge(&mut base, overlay);
        assert_eq!(
            base["gateway"]["client_port"].as_integer(),
            Some(9090),
            "overlay key should win"
        );
        assert_eq!(
            base["gateway"]["admin_port"].as_integer(),
            Some(8081),
            "base-only key should be preserved"
        );
    }

    #[test]
    fn deep_merge_array_replaces_same_named_entry() {
        let mut base: toml::Value = toml::from_str(
            r#"
[[tiers]]
name    = "local:fast"
model   = "qwen2.5:1.5b"
backend = "ollama"

[[tiers]]
name    = "local:deep"
model   = "qwen2.5:7b"
backend = "ollama"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[[tiers]]
name    = "local:fast"
model   = "qwen3:1.7b"
backend = "ollama"
"#,
        )
        .unwrap();
        deep_merge(&mut base, overlay);
        let tiers = base["tiers"].as_array().unwrap();
        assert_eq!(tiers.len(), 2, "should not append a duplicate name");
        let fast = &tiers[0];
        assert_eq!(
            fast["model"].as_str(),
            Some("qwen3:1.7b"),
            "overlay model should win"
        );
        // Second entry must be untouched.
        assert_eq!(tiers[1]["name"].as_str(), Some("local:deep"));
    }

    #[test]
    fn deep_merge_array_appends_new_named_entry() {
        let mut base: toml::Value = toml::from_str(
            r#"
[[tiers]]
name    = "local:fast"
model   = "qwen2.5:1.5b"
backend = "ollama"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[[tiers]]
name    = "local:deep"
model   = "qwen2.5:7b"
backend = "ollama"
"#,
        )
        .unwrap();
        deep_merge(&mut base, overlay);
        let tiers = base["tiers"].as_array().unwrap();
        assert_eq!(tiers.len(), 2, "new name should be appended");
        assert_eq!(tiers[1]["name"].as_str(), Some("local:deep"));
    }

    #[test]
    fn conf_d_overrides_backend_url() {
        let uid = uuid::Uuid::new_v4().to_string().replace('-', "");
        let dir = std::env::temp_dir().join(format!("lmg-test-{uid}"));
        let conf_d = dir.join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();

        let base_toml = r#"
[gateway]
client_port = 8080
admin_port  = 8081
traffic_log_capacity = 500

[backends.ollama]
base_url = "http://localhost:11434"

[[tiers]]
name    = "local:fast"
backend = "ollama"
model   = "qwen2.5:1.5b"

[aliases]
"hint:fast" = "local:fast"

[profiles.default]
mode          = "dispatch"
classifier    = "local:fast"
max_auto_tier = "local:fast"
"#;
        let override_toml = r#"
[backends.ollama]
base_url = "http://192.168.1.50:11434"
"#;
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, base_toml).unwrap();
        std::fs::write(conf_d.join("10-local.toml"), override_toml).unwrap();

        let config = Config::load(&cfg_path).expect("should load with conf.d override");
        assert_eq!(
            config.backends["ollama"].base_url,
            "http://192.168.1.50:11434"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn conf_d_is_silently_skipped_when_absent() {
        let uid = uuid::Uuid::new_v4().to_string().replace('-', "");
        let dir = std::env::temp_dir().join(format!("lmg-test-{uid}"));
        std::fs::create_dir_all(&dir).unwrap();

        let base_toml = r#"
[gateway]
client_port = 8080
admin_port  = 8081
traffic_log_capacity = 500

[backends.ollama]
base_url = "http://localhost:11434"

[[tiers]]
name    = "local:fast"
backend = "ollama"
model   = "qwen2.5:1.5b"

[profiles.default]
mode          = "dispatch"
classifier    = "local:fast"
max_auto_tier = "local:fast"
"#;
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, base_toml).unwrap();
        // No conf.d/ dir created — should not error.
        let config = Config::load(&cfg_path).expect("should load without conf.d");
        assert_eq!(config.gateway.client_port, 8080);

        std::fs::remove_dir_all(&dir).ok();
    }
}
