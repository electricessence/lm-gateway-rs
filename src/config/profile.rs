//! Routing tier and profile configuration types.

use serde::{Deserialize, Serialize};

/// A routing tier — a named combination of backend + model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TierConfig {
    /// Unique tier name, e.g. `local:fast`, `cloud:economy`.
    pub name: String,

    /// Which backend to use (must exist in `[backends]`).
    pub backend: String,

    /// Model name to send to the backend.
    pub model: String,

    /// If set, inject `"think": <value>` into every request forwarded to this tier.
    /// Primarily for Ollama backends: `false` disables chain-of-thought (faster),
    /// `true` enables it (slower but deeper reasoning). Absent = no injection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
}

/// Routing profile — controls routing behaviour for a client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileConfig {
    /// Routing mode.
    #[serde(default)]
    pub mode: RoutingMode,

    /// Fallback tier used when the model hint does not match any known tier or alias.
    /// In `classify` mode, this tier is also used for the pre-flight classification call.
    pub classifier: String,

    /// Highest tier auto-escalation can reach without an explicit override.
    pub max_auto_tier: String,

    /// If true, the `cloud:expert` tier (or highest tier) requires an explicit
    /// `"tier": "expert"` field in the request body or a custom header.
    #[serde(default)]
    pub expert_requires_flag: bool,

    /// Maximum requests per minute shared across **all** clients that resolve
    /// to this profile (default: unlimited).
    ///
    /// This is a profile-wide quota — not per-client-key. A value of 0 or
    /// absent means no per-profile limit; the global `gateway.rate_limit_rpm`
    /// (per-IP) still applies independently.
    #[serde(default)]
    pub rate_limit_rpm: Option<u32>,

    /// System prompt used by `classify` mode to ask the classifier tier for a
    /// routing label. The user message is appended verbatim after this prompt.
    ///
    /// Respond should be exactly one of: `simple`, `moderate`, or `complex`.
    /// Defaults to [`DEFAULT_CLASSIFIER_PROMPT`] if not set.
    #[serde(default)]
    pub classifier_prompt: Option<String>,

    /// Optional system prompt prepended to every request forwarded through this profile.
    ///
    /// When set, this text is injected as a `role = "system"` message at the front of
    /// the `messages` array before dispatching to the backend. If the request already
    /// includes a system message, the profile prompt is prepended to it (separated by
    /// `\n\n`), preserving any client-provided context while ensuring the profile's
    /// instructions always take precedence.
    ///
    /// Useful for per-profile personas, domain constraints, or output-format rules.
    ///
    /// ```toml
    /// [profiles.ha-auto]
    /// system_prompt = "You are a smart home assistant integrated with Home Assistant. ..."
    /// ```
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// Default classification prompt injected as the system message for `classify` mode.
///
/// Returns one of `instant`, `fast`, or `deep` — the router maps these to the
/// first, middle, and last tier in the profile's auto range respectively.
/// These labels are also accepted as synonyms for the legacy `simple`/`moderate`/`complex`
/// vocabulary, so existing configs continue to work unchanged.
///
/// This is the v16 prompt (27/27 on a 27-case HA benchmark against qwen3:1.7b).
/// Expanded from v10 with explicit multi-device + polite-combo instant examples
/// to fix false-positive fast-think on "Could you dim X and set Y" style requests.
/// Override per-profile with `classifier_prompt` if your workload needs a different rubric.
pub const DEFAULT_CLASSIFIER_PROMPT: &str = "\
Classify the request with one word.\n\
\n\
Labels:\n\
  instant        = device commands, simple state queries, one-sentence answers\n\
  instant-think  = instant tier but needs brief reasoning\n\
  fast           = multi-step commands, explanations, one-paragraph answers\n\
  fast-think     = fast tier requiring reasoning (e.g. creating an automation)\n\
  deep           = long-form output, multi-paragraph answers\n\
  deep-think     = deep tier requiring complex reasoning (e.g. debugging YAML)\n\
\n\
Turn on the kitchen lights -> instant\n\
Lock the front door -> instant\n\
Set the thermostat to 72 -> instant\n\
Is the garage door open? -> instant\n\
What is the living room temperature? -> instant\n\
What is the temperature outside? -> instant\n\
Are any lights on downstairs? -> instant\n\
Dim the bedroom lights to 40% and set the AC to 68 -> instant\n\
Turn off the office lights and lock the front door -> instant\n\
Create an automation to turn off lights when I leave -> fast-think\n\
Set up a sunset porch light automation -> fast-think\n\
Why does my lights automation run twice every morning? -> deep-think\n\
My away mode isn't triggering, here is the YAML -> deep-think\n\
\n\
Reply with one word only.";

/// How the routing decision is made.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// Route directly to the tier matching the `model` hint in the request body.
    ///
    /// Aliases are resolved first (`hint:fast` → `local:fast`). Unknown hints
    /// fall back to the `classifier` tier. No extra inference calls are made.
    #[default]
    Dispatch,

    /// Try each tier from cheapest upward. Return the first sufficient response.
    ///
    /// "Sufficient" is determined by heuristics (response length, absence of
    /// refusal phrases). Reduces cost for simple queries.
    Escalate,

    /// Make a fast pre-flight call to the `classifier` tier to determine request
    /// complexity, then dispatch to the appropriate tier.
    ///
    /// The classifier responds with one word (`simple`, `moderate`, or `complex`),
    /// which is mapped to the first, middle, or last tier in the profile's auto
    /// range (up to `max_auto_tier`). Adds ~200–600 ms latency before the main
    /// inference call begins.
    Classify,
}

impl std::fmt::Display for RoutingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dispatch => "dispatch",
            Self::Escalate => "escalate",
            Self::Classify => "classify",
        })
    }
}
