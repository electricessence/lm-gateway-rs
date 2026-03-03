//! Classification helpers — pure functions for parsing and resolving tier labels.
//!
//! Separated so the same label-to-tier logic can be shared between
//! [`modes::classify_and_dispatch`] (non-streaming) and the classify branch of
//! [`super::route_stream`] without duplication.

use std::collections::HashMap;

use serde_json::Value;
use tracing::debug;

use crate::config::TierConfig;

/// Full result of parsing a structured or plain-text classifier response.
///
/// Structured format: `"tier=fast intent=greeting domain=home"`
/// Legacy format:     `"fast"` or `"deep-think"`
#[derive(Debug, Default)]
pub(crate) struct ParsedClassification {
    /// The resolved tier label, e.g. `"fast"` (never includes `-think`).
    pub tier_label: String,
    /// `Some(true)` when the `-think` suffix was present; `None` otherwise.
    pub think_override: Option<bool>,
    /// All `key=value` pairs emitted by the classifier, including `tier`.
    pub tags: HashMap<String, String>,
}

/// Parse a full structured or plain-text classification response.
///
/// Handles both the legacy single-token format (`"fast"`) and the richer
/// structured format (`"tier=fast intent=greeting domain=home"`).
/// Tokens containing `=` are inserted into `tags`; bare tokens are treated as
/// candidate tier labels.  `tier_label` is taken from `tags["tier"]` if
/// present, otherwise from the first bare token, otherwise falls back to
/// `"instant"`.
pub(crate) fn parse_classification(response: &Value) -> ParsedClassification {
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let mut tags: HashMap<String, String> = HashMap::new();
    let mut bare_tokens: Vec<String> = Vec::new();

    for token in content.split_whitespace() {
        if let Some((key, val)) = token.split_once('=') {
            let key = key.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
            let val = val.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
            if !key.is_empty() && !val.is_empty() {
                tags.insert(key.to_owned(), val.to_owned());
            }
        } else {
            let s = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
            if !s.is_empty() {
                bare_tokens.push(s.to_owned());
            }
        }
    }

    let raw_label = tags
        .get("tier")
        .cloned()
        .or_else(|| bare_tokens.into_iter().next())
        .unwrap_or_else(|| "instant".to_owned());

    let (tier_label, think_override) = if let Some(s) = raw_label.strip_suffix("-think") {
        (s.to_owned(), Some(true))
    } else {
        (raw_label, None)
    };

    ParsedClassification { tier_label, think_override, tags }
}

/// Parse a classification label from the classifier's response.
///
/// Returns the base label token (lowercased, punctuation-stripped) and an
/// optional think override.  Delegates to [`parse_classification`] so that both
/// the legacy single-token format and the richer structured format are handled
/// identically.
///
/// The `-think` suffix (e.g. `deep-think`) sets `think_override = Some(true)`.
pub(crate) fn parse_classification_label(response: &Value) -> (String, Option<bool>) {
    let p = parse_classification(response);
    (p.tier_label, p.think_override)
}

/// Resolve a classifier label string to a tier from the candidate slice.
///
/// Matching is purely name-driven so the routing behaviour is defined entirely
/// by config — no Rust changes are needed when tiers are added, removed, or
/// renamed.
///
/// Matching order:
/// 1. Exact tier name match       (e.g. `"local:instant"`)
/// 2. Tier name suffix after `:`  (e.g. `"instant"` → `"local:instant"`)
/// 3. Unknown label → middle tier (safe centre-ground fallback)
///
/// The classifier prompt (`profile.classifier_prompt`) is the contract that
/// tells the model which labels to use.  The default prompt uses tier name
/// suffixes, so new tiers are automatically routable by updating the prompt.
pub(crate) fn resolve_tier_by_label<'a>(label: &str, candidates: &'a [TierConfig]) -> &'a TierConfig {
    let n = candidates.len();
    // 1. Exact full name (e.g. "local:instant").
    if let Some(t) = candidates.iter().find(|t| t.name == label) {
        return t;
    }
    // 2. Suffix after the last ':' (e.g. "instant" matches "local:instant").
    if let Some(t) = candidates.iter().find(|t| t.name.rsplit(':').next() == Some(label)) {
        return t;
    }
    // 3. Unknown label — fall back to the middle tier as a safe default.
    //    If a classifier returns an unrecognised word, middle is a reasonable
    //    centre-ground: not the cheapest, not the most expensive.
    debug!(label, "unrecognised classification label — falling back to middle tier");
    &candidates[n / 2]
}
