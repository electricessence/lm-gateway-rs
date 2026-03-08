//! Per-tier priority gate — "fire if top, queue if not".
//!
//! A request with priority `P` fires immediately if `P > max(in_flight)`.
//! Otherwise it waits in a FIFO queue (within priority level) until an
//! in-flight request completes and re-evaluation succeeds.
//!
//! # Priority scale
//! | Value  | Meaning                                 |
//! |--------|-----------------------------------------|
//! | `+N`   | Higher than normal — served first       |
//! | `0`    | Normal (default)                        |
//! | `-N`   | Background — queued behind everything   |
//!
//! # Provider policy
//! Local providers (Ollama, OpenAI-compat) use this gate. Cloud providers
//! (Anthropic, OpenRouter) bypass it — the cloud manages its own queue.

use std::sync::Arc;

use axum::http::HeaderMap;
use tokio::sync::Mutex;

/// Default priority for requests that omit `X-LMG-Priority`.
pub const DEFAULT_PRIORITY: i32 = 0;

/// Parse the `X-LMG-Priority` header as an `i32`.
///
/// Returns [`DEFAULT_PRIORITY`] when the header is absent or contains a
/// non-integer value.
pub fn parse_priority(headers: &HeaderMap) -> i32 {
    headers
        .get("x-lmg-priority")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(DEFAULT_PRIORITY)
}

// ---------------------------------------------------------------------------
// Internal gate state
// ---------------------------------------------------------------------------

struct PendingEntry {
    priority: i32,
    ticket: u64,
    tx: tokio::sync::oneshot::Sender<()>,
}

struct GateState {
    /// Priorities of all currently in-flight requests (may have duplicates).
    in_flight: Vec<i32>,
    /// Pending entries sorted: highest priority first, FIFO within same level.
    pending: Vec<PendingEntry>,
    next_ticket: u64,
}

impl GateState {
    /// Returns `true` when `priority` is strictly greater than every in-flight priority.
    ///
    /// An empty in-flight set always allows firing (vacuously true: nothing to beat).
    fn can_fire(&self, priority: i32) -> bool {
        self.in_flight
            .iter()
            .copied()
            .max()
            .is_none_or(|max| priority > max)
    }

    /// Try to unblock the next eligible pending entry.
    ///
    /// Cancelled entries (dropped receivers) are cleaned up lazily here.
    /// Only the front of the queue is evaluated — if it can't fire, nothing
    /// else will be woken up either, preserving priority ordering.
    fn try_unblock_next(&mut self) {
        while let Some(front) = self.pending.first() {
            if front.tx.is_closed() {
                // Waiter was cancelled — remove it and try the next.
                self.pending.remove(0);
                continue;
            }
            if self.can_fire(front.priority) {
                let entry = self.pending.remove(0);
                self.in_flight.push(entry.priority);
                let _ = entry.tx.send(());
            }
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Per-tier priority gate.
///
/// Construct once per tier at gateway startup and store in [`super::RouterState`].
/// The gate is cheap to clone — all clones share the same internal state.
///
/// # Scheduling algorithm
/// ```text
/// on arrival(P):
///     if P > max(in_flight):  fire immediately
///     else:                   enqueue, sorted by priority DESC then arrival ASC
///
/// on completion:
///     remove from in_flight
///     if pending.front can_fire:  unblock it
/// ```
#[derive(Clone)]
pub struct TierPriorityGate {
    state: Arc<Mutex<GateState>>,
}

impl TierPriorityGate {
    /// Create a new gate with an empty in-flight set.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(GateState {
                in_flight: Vec::new(),
                pending: Vec::new(),
                next_ticket: 0,
            })),
        }
    }

    /// Acquire an in-flight slot for a request with the given `priority`.
    ///
    /// Returns immediately if the request can fire, or suspends until a
    /// completing request unblocks it. The returned [`PriorityPermit`]
    /// releases the slot when dropped.
    pub async fn acquire(&self, priority: i32) -> PriorityPermit {
        // Fast path: check under the lock whether we can fire immediately.
        let rx = {
            let mut state = self.state.lock().await;
            if state.can_fire(priority) {
                state.in_flight.push(priority);
                return PriorityPermit {
                    state: Arc::clone(&self.state),
                    priority,
                };
            }

            // Slow path: register in the pending queue and wait for a signal.
            let ticket = state.next_ticket;
            state.next_ticket += 1;
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            state.pending.push(PendingEntry { priority, ticket, tx });
            // Re-sort on every insert to keep highest-priority entries at front.
            // Hot-path caches should be small (typically < 10 entries) so this
            // is cheaper than a BinaryHeap with the reverse-lookup overhead.
            state
                .pending
                .sort_unstable_by(|a, b| b.priority.cmp(&a.priority).then(a.ticket.cmp(&b.ticket)));
            rx
            // Lock released here — the sender for `rx` is now stored in `pending`.
        };

        // Wait for the unblock signal sent by a completing request's Drop.
        // Oneshot guarantees that a value sent before this await is received
        // immediately, so there is no lost-wakeup race.
        let _ = rx.await;

        PriorityPermit {
            state: Arc::clone(&self.state),
            priority,
        }
    }
}

/// RAII guard that releases an in-flight slot on drop.
///
/// Releasing the slot triggers re-evaluation of the pending queue: if the
/// highest-priority waiter can now fire, it is unblocked.
pub struct PriorityPermit {
    state: Arc<Mutex<GateState>>,
    priority: i32,
}

impl Drop for PriorityPermit {
    fn drop(&mut self) {
        let state = Arc::clone(&self.state);
        let priority = self.priority;
        tokio::spawn(async move {
            let mut s = state.lock().await;
            // Remove one occurrence of this priority from in_flight (swap_remove
            // is O(1) and order doesn't matter for a multiset).
            if let Some(pos) = s.in_flight.iter().position(|&p| p == priority) {
                s.in_flight.swap_remove(pos);
            }
            s.try_unblock_next();
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_gate_fires_any_priority() {
        let gate = TierPriorityGate::new();
        // Even a very negative priority fires immediately when nothing is in-flight.
        let _permit = gate.acquire(-999).await;
    }

    #[tokio::test]
    async fn high_priority_fires_immediately_over_lower_in_flight() {
        let gate = TierPriorityGate::new();
        let _low = gate.acquire(0).await; // 0 is in-flight
        // 100 > 0 → should not block
        let start = std::time::Instant::now();
        let _high = gate.acquire(100).await;
        assert!(
            start.elapsed().as_millis() < 50,
            "higher-priority acquire should not block"
        );
    }

    #[tokio::test]
    async fn equal_priority_queues_behind_in_flight() {
        let gate = TierPriorityGate::new();
        let first = gate.acquire(0).await;
        // Second at same priority: 0 is NOT > 0, so it must queue.
        let gate2 = gate.clone();
        let handle = tokio::spawn(async move { gate2.acquire(0).await });
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        assert!(!handle.is_finished(), "second acquire should be blocked");
        drop(first);
        let _second = handle.await.expect("second acquire must succeed after first drops");
    }

    #[tokio::test]
    async fn background_queues_behind_normal_in_flight() {
        let gate = TierPriorityGate::new();
        let normal = gate.acquire(0).await;
        // -100 is NOT > 0 → must queue.
        let gate2 = gate.clone();
        let handle = tokio::spawn(async move { gate2.acquire(-100).await });
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        assert!(!handle.is_finished(), "background must be blocked");
        drop(normal);
        let _bg = handle.await.expect("background fires once normal completes");
    }

    #[tokio::test]
    async fn high_priority_jumps_queue_of_waiting_background() {
        let gate = TierPriorityGate::new();
        // A "normal" request is in-flight.
        let _normal = gate.acquire(0).await;
        // Background starts waiting.
        let gate2 = gate.clone();
        let bg_handle = tokio::spawn(async move { gate2.acquire(-50).await });
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        // High-priority arrives — should fire immediately (100 > 0 = true).
        let start = std::time::Instant::now();
        let _high = gate.acquire(100).await;
        assert!(start.elapsed().as_millis() < 50, "high priority must not block");
        // Background is still waiting (held back by _normal and now _high).
        assert!(!bg_handle.is_finished(), "background still queued behind high");
        // Cleanup
        bg_handle.abort();
    }
}
