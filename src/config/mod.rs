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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&content).context("parsing config TOML")?;
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
}
