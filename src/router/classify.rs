//! Classification helpers — pure functions for parsing and resolving tier labels.
//!
//! Separated so the same label-to-tier logic can be shared between
//! [`modes::classify_and_dispatch`] (non-streaming) and the classify branch of
//! [`super::route_stream`] without duplication.

use serde_json::Value;
use tracing::debug;

use crate::config::TierConfig;

/// Parse a classification label from the classifier's response.
///
/// Returns the base label token (lowercased, punctuation-stripped) and an
/// optional think override.  The label is kept as a plain `String` so that
/// tier resolution matches against configured tier names without any Rust
/// changes when tiers are added, removed, or renamed in config.
///
/// The `-think` suffix (e.g. `deep-think`) sets `think_override = Some(true)`.
pub(crate) fn parse_classification_label(response: &Value) -> (String, Option<bool>) {
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let first = content
        .split_whitespace()
        .next()
        .unwrap_or("")
        // Strip leading/trailing punctuation so "simple." or "[deep]" still match.
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '-');

    // Detect the -think suffix: deep-think, max-think, instant-think, etc.
    if let Some(stripped) = first.strip_suffix("-think") {
        (stripped.to_owned(), Some(true))
    } else {
        (first.to_owned(), None)
    }
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
