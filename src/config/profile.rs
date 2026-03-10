//! Routing tier and profile configuration types.

use std::collections::HashMap;

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

    /// Context window size (in tokens) for this tier's model.
    ///
    /// When set, the router estimates the incoming request's token count and
    /// skips this tier if the estimate exceeds the window — automatically
    /// bumping the request to the next tier that can fit it. This prevents
    /// silent truncation at the model level.
    ///
    /// Leave unset to disable context-window gating for this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u32>,
}

/// A routing rule evaluated against semantic classification tags.
///
/// Rules are sorted by `priority` descending and evaluated in order. The first
/// rule whose `when` map is a subset of the request's classification tags wins.
///
/// **Config example**
/// ```toml
/// [[profiles.default.rules]]
/// when     = { intent = "greeting" }
/// route_to = "local:instant"
/// priority = 30
///
/// [[profiles.default.rules]]
/// when     = { intent = "command", domain = "home" }
/// route_to = "local:fast"
/// priority = 20
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuleConfig {
    /// All key=value pairs in this map must match the classifier's tags for the
    /// rule to fire. Matching is case-insensitive.
    pub when: HashMap<String, String>,

    /// Tier name (or alias) to route to when this rule matches.
    pub route_to: String,

    /// Evaluation order: higher priority rules are checked first. Default: 0.
    #[serde(default)]
    pub priority: i32,
}

/// Routing profile — controls routing behaviour for a client.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProfileConfig {
    /// Routing mode.
    #[serde(default)]
    pub mode: RoutingMode,

    /// Fallback tier used when the model hint does not match any known tier or alias.
    /// In `classify` mode, this tier is also used for the pre-flight classification call.
    #[serde(default)]
    pub classifier: String,

    /// Highest tier auto-escalation can reach without an explicit override.
    #[serde(default)]
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
    /// The classifier responds with one word (e.g. `instant`, `fast`, `deep`).
    /// Defaults to [`DEFAULT_CLASSIFIER_PROMPT`] if not set.
    #[serde(default)]
    pub classifier_prompt: Option<String>,

    /// Override the `think` flag sent to the classifier model.
    ///
    /// When `true`, chain-of-thought reasoning is enabled for the pre-flight
    /// classification call (more accurate but ~2–4 s slower). When `false`
    /// (default), thinking is disabled for the fastest possible pre-flight.
    /// When absent, defaults to `false`.
    #[serde(default)]
    pub classifier_think: Option<bool>,

    /// Timeout in milliseconds for the classifier pre-flight call. Default: 10 000 (10 s).
    ///
    /// Classification should be fast (typically 200–600 ms). This timeout prevents
    /// a stuck backend from blocking the entire request for minutes. When the
    /// timeout fires, the classifier defaults to the first tier (cheapest).
    #[serde(default = "super::gateway::defaults::classifier_timeout_ms")]
    pub classifier_timeout_ms: u64,

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

    /// Tag-based routing rules evaluated before tier resolution in `classify` mode.
    ///
    /// Rules are sorted by `priority` descending. The first rule whose `when` tags
    /// are all present in the classifier's response wins and the request is dispatched
    /// directly to `route_to`, bypassing normal tier-ladder resolution.
    ///
    /// An empty (or absent) `rules` list leaves behaviour identical to no-rules mode.
    #[serde(default)]
    pub rules: Vec<RuleConfig>,

    /// Per-class system prompt overrides, keyed by the classification label
    /// returned by the classifier (e.g. `"command"`, `"inquiry"`, `"greeting"`).
    ///
    /// When the classifier assigns a label, the matching prompt is prepended to the
    /// request's system message immediately before dispatch — ahead of the profile-level
    /// `system_prompt` (so the class-specific text takes higher precedence).
    /// Enables per-intent framing without duplicating the main prompt.
    ///
    /// ```toml
    /// [profiles.ha-auto.class_prompts]
    /// command      = "Execute the home action directly. Confirm briefly."
    /// conversation = "The user is responding to your prior question. Use the conversation history."
    /// inquiry      = "The user is asking about device state. Query and respond in one sentence."
    /// ```
    #[serde(default)]
    pub class_prompts: HashMap<String, String>,

    /// How many recent user+assistant messages to include in the classifier input.
    ///
    /// By default the classifier sees the full conversation history (all user and
    /// assistant messages, excluding system and tool messages). Set this to a
    /// positive integer to cap how far back the classifier looks — e.g. `4` sends
    /// only the last 4 user/assistant messages.
    ///
    /// Setting `0` disables classification entirely for the profile: the router
    /// skips the classifier step and routes starting at the classifier tier,
    /// still applying context-window gating (which may bump the request to a
    /// higher tier if the classifier tier cannot fit the request).
    ///
    /// Useful for profiles where classification speed matters more than deep
    /// context awareness (e.g. Home Assistant voice, where each utterance is
    /// mostly self-contained).
    #[serde(default)]
    pub classifier_context: Option<u32>,

    /// Static response text returned by `reply` mode without calling any backend.
    ///
    /// Defaults to a generic "not configured" message when absent.
    #[serde(default)]
    pub reply_message: Option<String>,
}

/// Default classification prompt injected as the system message for `classify` mode.
///
/// Returns one of `instant`, `fast`, or `deep` — the router maps these to the
/// first, middle, and last tier in the profile's auto range respectively.
/// The label vocabulary is defined by the prompt — override per-profile via
/// `classifier_prompt` to use any labels your workload needs.
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
    /// The classifier responds with one word (e.g. `instant`, `fast`, `deep`),
    /// which is mapped to the appropriate tier in the profile's auto range
    /// (up to `max_auto_tier`). Adds ~200–600 ms latency before the main
    /// inference call begins.
    Classify,

    /// Return a static response without calling any backend.
    ///
    /// The response text comes from [`ProfileConfig::reply_message`] (or a
    /// default). Useful as a dead-end for overflow/escalation profiles that
    /// are not yet wired to a backend — the profile can later be reconfigured
    /// to `classify` or `dispatch` when cloud models are added.
    Reply,
}

impl std::fmt::Display for RoutingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dispatch => "dispatch",
            Self::Escalate => "escalate",
            Self::Classify => "classify",
            Self::Reply => "reply",
        })
    }
}
