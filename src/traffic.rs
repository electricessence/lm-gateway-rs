//! In-memory traffic log exposed through the admin API.
//!
//! [`TrafficLog`] is a fixed-capacity ring-buffer: once full, the oldest entry
//! is evicted to make room for the newest. This gives a bounded, O(1) memory
//! footprint regardless of request volume.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[cfg(feature = "debug-traffic")]
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Fixed-capacity ring-buffer of recent [`TrafficEntry`] records.
///
/// Safe to share across threads via `Arc<TrafficLog>`. [`push`][Self::push] uses
/// a non-blocking `try_lock` so it never delays request handling; in the
/// unlikely event of lock contention the entry is silently dropped.
pub struct TrafficLog {
    capacity: usize,
    entries: Mutex<VecDeque<TrafficEntry>>,
}

impl TrafficLog {
    /// Create a new log with the given capacity.
    ///
    /// `capacity` is the maximum number of entries retained. Older entries are
    /// silently dropped once the buffer is full.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }

    /// Record a completed request.
    ///
    /// This is a best-effort, non-blocking operation: if the mutex is contended
    /// the entry is dropped rather than blocking the request path.
    pub fn push(&self, entry: TrafficEntry) {
        // Best-effort non-blocking push — drop if lock contention
        if let Ok(mut entries) = self.entries.try_lock() {
            if entries.len() == self.capacity {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
    }

    /// Return up to `limit` recent entries, newest first.
    pub async fn recent(&self, limit: usize) -> Vec<TrafficEntry> {
        let entries = self.entries.lock().await;
        entries
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Compute public-safe aggregate statistics (no tier/backend names).
    pub async fn public_stats(&self) -> PublicStats {
        let entries = self.entries.lock().await;
        let total = entries.len();
        let avg_latency_ms = if total == 0 {
            0.0
        } else {
            entries.iter().map(|e| e.latency_ms as f64).sum::<f64>() / total as f64
        };
        let error_count = entries.iter().filter(|e| !e.success).count();
        let escalation_count = entries.iter().filter(|e| e.escalated).count();
        PublicStats { total_requests: total, error_count, escalation_count, avg_latency_ms }
    }

    /// Compute aggregate statistics over all buffered entries.
    pub async fn stats(&self) -> TrafficStats {
        let entries = self.entries.lock().await;
        let total = entries.len();
        let avg_latency_ms = if total == 0 {
            0.0
        } else {
            entries.iter().map(|e| e.latency_ms as f64).sum::<f64>() / total as f64
        };

        let error_count = entries.iter().filter(|e| !e.success).count();
        let escalation_count = entries.iter().filter(|e| e.escalated).count();

        let mut tier_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for entry in entries.iter() {
            *tier_counts.entry(entry.tier.clone()).or_default() += 1;
        }

        TrafficStats {
            total_requests: total,
            error_count,
            escalation_count,
            avg_latency_ms,
            tier_counts,
        }
    }

    /// Compute per-backend health from the most recent `window` entries for each backend.
    ///
    /// Returns a map from backend name to [`BackendHealthStats`].  Backends with no
    /// traffic in the window are not included — callers should treat a missing entry
    /// as healthy (no evidence of failure yet).
    ///
    /// A minimum of 3 samples is required before a backend can be classified as
    /// unhealthy, to avoid false positives for rarely-used or newly-added backends.
    pub async fn backend_health(
        &self,
        window: usize,
        threshold: f64,
    ) -> std::collections::HashMap<String, BackendHealthStats> {
        let entries = self.entries.lock().await;
        // Iterate newest-first, collecting up to `window` outcomes per backend.
        let mut by_backend: std::collections::HashMap<String, Vec<bool>> =
            std::collections::HashMap::new();
        for entry in entries.iter().rev() {
            let bucket = by_backend.entry(entry.backend.clone()).or_default();
            if bucket.len() < window {
                bucket.push(entry.success);
            }
        }
        by_backend
            .into_iter()
            .map(|(backend, outcomes)| {
                let total = outcomes.len();
                let errors = outcomes.iter().filter(|&&ok| !ok).count();
                let error_rate = if total == 0 {
                    0.0
                } else {
                    errors as f64 / total as f64
                };
                // Require at least 3 samples before marking a backend unhealthy.
                let healthy = total < 3 || error_rate < threshold;
                (backend, BackendHealthStats { total, errors, error_rate, healthy })
            })
            .collect()
    }
}

/// A single request record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficEntry {
    /// Unique request ID.
    pub id: String,
    /// Timestamp of the request.
    pub timestamp: DateTime<Utc>,
    /// Active profile at the time of the request.
    pub profile: Option<String>,
    /// Original model alias or tier name from the request body.
    pub requested_model: Option<String>,
    /// Tier that ultimately handled this request.
    pub tier: String,
    /// Backend that handled this request.
    pub backend: String,
    /// Routing mode applied (`"dispatch"` or `"escalate"`).
    pub routing_mode: Option<String>,
    /// Whether the request was escalated to a higher tier during routing.
    pub escalated: bool,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the backend returned a success response.
    pub success: bool,
    /// Error description when `success` is `false`.
    pub error: Option<String>,
    /// Classification class label (e.g. `"greeting"`, `"command"`).
    /// Populated only when the profile uses `mode = "classify"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_label: Option<String>,
    /// Ordered list of profiles traversed during cascade routing.
    /// A single-hop request has exactly one entry (the initial profile).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_chain: Option<Vec<String>>,
    /// Scheduling priority from the `X-LMG-Priority` request header.
    /// `0` = normal (default), `+N` = higher, `-N` = background.
    #[serde(default)]
    pub priority: i32,
    /// Full request body captured for debugging.
    ///
    /// Only populated when the `debug-traffic` Cargo feature is compiled in
    /// **and** `traffic_log_debug = true` in the `[gateway]` config.
    /// Contains messages, tools, and system prompt as dispatched to the backend.
    #[cfg(feature = "debug-traffic")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_request_body: Option<Value>,
}

impl TrafficEntry {
    pub fn new(tier: String, backend: String, latency_ms: u64, success: bool) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            profile: None,
            requested_model: None,
            tier,
            backend,
            routing_mode: None,
            escalated: false,
            latency_ms,
            success,
            error: None,
            class_label: None,
            profile_chain: None,
            priority: 0,
            #[cfg(feature = "debug-traffic")]
            debug_request_body: None,
        }
    }

    /// Attach the active profile name.
    pub fn with_profile(mut self, profile: &str) -> Self {
        self.profile = Some(profile.to_string());
        self
    }

    /// Attach the original model hint from the request.
    pub fn with_requested_model(mut self, model: &str) -> Self {
        self.requested_model = Some(model.to_string());
        self
    }

    /// Attach the routing mode string (`"dispatch"` or `"escalate"`).
    pub fn with_routing_mode(mut self, mode: &str) -> Self {
        self.routing_mode = Some(mode.to_string());
        self
    }

    /// Mark this entry as having been escalated to a higher tier.
    pub fn mark_escalated(mut self) -> Self {
        self.escalated = true;
        self
    }

    /// Attach an error description for failed requests.
    pub fn with_error(mut self, err: &str) -> Self {
        self.error = Some(err.to_string());
        self
    }

    /// Override the auto-generated UUID with a specific ID.
    ///
    /// Used to unify the `TrafficEntry` ID with the inbound `X-Request-ID`,
    /// so the admin traffic view, log output, and client response headers all
    /// reference the same identifier.
    pub fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }

    /// Attach the scheduling priority from `X-LMG-Priority`.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Attach the full request body for debugging.
    ///
    /// Only available when compiled with `--features debug-traffic`.
    #[cfg(feature = "debug-traffic")]
    pub fn with_debug_messages(mut self, body: &Value) -> Self {
        self.debug_request_body = Some(body.clone());
        self
    }

    /// Attach routing trace from a classify-mode resolution.
    ///
    /// Records the class label (e.g. `"greeting"`) and the ordered chain of
    /// profiles traversed during cascade routing. Used to populate
    /// `X-LMG-Class` and `X-LMG-Profile` response headers.
    pub fn with_routing_trace(mut self, class_label: String, profile_chain: Vec<String>) -> Self {
        if !class_label.is_empty() {
            self.class_label = Some(class_label);
        }
        if !profile_chain.is_empty() {
            self.profile_chain = Some(profile_chain);
        }
        self
    }
}

/// Aggregate statistics derived from all buffered [`TrafficEntry`] records.
#[derive(Debug, Serialize)]
pub struct TrafficStats {
    pub total_requests: usize,
    /// Number of requests that returned an error.
    pub error_count: usize,
    /// Number of requests that were escalated to a higher tier.
    pub escalation_count: usize,
    pub avg_latency_ms: f64,
    pub tier_counts: std::collections::HashMap<String, usize>,
}

/// Public-safe aggregate statistics — no backend or tier names included.
///
/// Safe to return from an unauthenticated endpoint: contains only counts and
/// latency data, never any configuration detail that could reveal infrastructure.
#[derive(Debug, Serialize)]
pub struct PublicStats {
    pub total_requests: usize,
    pub error_count: usize,
    pub escalation_count: usize,
    pub avg_latency_ms: f64,
}

/// Per-backend health summary derived from recent traffic entries.
///
/// Returned by [`TrafficLog::backend_health`]. A backend needs at least 3
/// samples in the window before it can be classified as unhealthy.
#[derive(Debug, Clone, Serialize)]
pub struct BackendHealthStats {
    /// Number of recent entries for this backend within the window.
    pub total: usize,
    /// Number of those entries that were errors.
    pub errors: usize,
    /// Error fraction in `[0.0, 1.0]`.
    pub error_rate: f64,
    /// `true` if the backend passes the health threshold (or has < 3 samples).
    pub healthy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(tier: &str, latency_ms: u64) -> TrafficEntry {
        TrafficEntry::new(tier.into(), "test-backend".into(), latency_ms, true)
    }

    // -----------------------------------------------------------------------
    // Basic push / read
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn push_and_retrieve_single_entry() {
        let log = TrafficLog::new(10);
        log.push(make_entry("local:fast", 42));

        let recent = log.recent(10).await;
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].tier, "local:fast");
        assert_eq!(recent[0].latency_ms, 42);
    }

    #[tokio::test]
    async fn recent_returns_entries_newest_first() {
        let log = TrafficLog::new(10);
        log.push(make_entry("local:fast", 1));
        log.push(make_entry("cloud:economy", 2));
        log.push(make_entry("cloud:standard", 3));

        let recent = log.recent(10).await;
        // Newest first
        assert_eq!(recent[0].tier, "cloud:standard");
        assert_eq!(recent[1].tier, "cloud:economy");
        assert_eq!(recent[2].tier, "local:fast");
    }

    #[tokio::test]
    async fn recent_limits_result_count() {
        let log = TrafficLog::new(20);
        for i in 0..10u64 {
            log.push(make_entry("local:fast", i));
        }
        let recent = log.recent(3).await;
        assert_eq!(recent.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Ring-buffer overflow
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oldest_entry_evicted_when_capacity_exceeded() {
        let log = TrafficLog::new(3);
        log.push(make_entry("oldest", 1));
        log.push(make_entry("middle", 2));
        log.push(make_entry("newest", 3));
        // This push should evict "oldest"
        log.push(make_entry("extra", 4));

        let all = log.recent(100).await;
        assert_eq!(all.len(), 3);
        // "oldest" must be gone
        assert!(!all.iter().any(|e| e.tier == "oldest"));
        // "extra" must be present
        assert!(all.iter().any(|e| e.tier == "extra"));
    }

    // -----------------------------------------------------------------------
    // Stats
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stats_on_empty_log() {
        let log = TrafficLog::new(10);
        let stats = log.stats().await;
        assert_eq!(stats.total_requests, 0);
        assert_eq!(stats.avg_latency_ms, 0.0);
        assert!(stats.tier_counts.is_empty());
    }

    #[tokio::test]
    async fn stats_averages_latency_correctly() {
        let log = TrafficLog::new(10);
        log.push(make_entry("local:fast", 100));
        log.push(make_entry("local:fast", 200));
        log.push(make_entry("cloud:economy", 300));

        let stats = log.stats().await;
        assert_eq!(stats.total_requests, 3);
        // Average: (100 + 200 + 300) / 3 = 200.0
        assert!((stats.avg_latency_ms - 200.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn stats_counts_requests_per_tier() {
        let log = TrafficLog::new(10);
        log.push(make_entry("local:fast", 10));
        log.push(make_entry("local:fast", 20));
        log.push(make_entry("cloud:economy", 30));

        let stats = log.stats().await;
        assert_eq!(stats.tier_counts["local:fast"], 2);
        assert_eq!(stats.tier_counts["cloud:economy"], 1);
    }

    // -----------------------------------------------------------------------
    // TrafficEntry fields
    // -----------------------------------------------------------------------

    #[test]
    fn entry_has_unique_ids() {
        let a = make_entry("local:fast", 1);
        let b = make_entry("local:fast", 1);
        assert_ne!(a.id, b.id, "every entry must have a unique UUID");
    }

    #[test]
    fn entry_records_success_flag() {
        let ok = TrafficEntry::new("t".into(), "b".into(), 0, true);
        let err = TrafficEntry::new("t".into(), "b".into(), 0, false);
        assert!(ok.success);
        assert!(!err.success);
    }

    // -----------------------------------------------------------------------
    // debug-traffic builder
    // -----------------------------------------------------------------------

    #[cfg(feature = "debug-traffic")]
    #[test]
    fn with_debug_messages_populates_debug_request_body() {
        use serde_json::json;
        let body = json!({
            "model": "hint:fast",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let entry = TrafficEntry::new("local:fast".into(), "mock".into(), 10, true)
            .with_debug_messages(&body);
        assert_eq!(
            entry.debug_request_body.as_ref(),
            Some(&body),
            "debug_request_body must equal the body passed to with_debug_messages"
        );
    }

    #[cfg(feature = "debug-traffic")]
    #[test]
    fn with_debug_messages_clones_independently() {
        use serde_json::json;
        let mut body = json!({"model": "hint:fast", "messages": []});
        let entry = TrafficEntry::new("local:fast".into(), "mock".into(), 10, true)
            .with_debug_messages(&body);
        // Mutate the original — the captured copy must be unaffected.
        body["model"] = json!("mutated");
        assert_eq!(
            entry.debug_request_body.as_ref().and_then(|b| b["model"].as_str()),
            Some("hint:fast")
        );
    }
}
