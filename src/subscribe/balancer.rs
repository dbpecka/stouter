//! Per-service backend selection: smooth weighted round-robin, passive
//! outlier ejection, and per-backend connection limits (circuit breaking).
//!
//! The drain and ejection filters fail open: if one would leave no
//! candidates, it is skipped so a connection can still try something rather
//! than fail outright. The connection limit is the exception: it is a hard
//! cap, since its whole point is to stop piling load onto a saturated backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::NodeInfo;

/// Consecutive tunnel-setup failures that eject a node.
const EJECT_AFTER_FAILURES: u32 = 3;

/// Ejection duration for a node's first ejection. Each further ejection
/// without an intervening success doubles it, up to [`MAX_EJECTION`].
const BASE_EJECTION: Duration = Duration::from_secs(10);

/// Upper bound on a single ejection.
const MAX_EJECTION: Duration = Duration::from_secs(120);

/// A node hosting the service, with its configured weight and limit.
#[derive(Debug, Clone)]
pub struct Backend {
    pub node: NodeInfo,
    pub weight: u32,
    /// Maximum concurrent connections (HTTP: in-flight requests) from this
    /// proxy; `None` is unlimited.
    pub max_connections: Option<u32>,
    /// HTTP only: time allowed for response headers.
    pub request_timeout: Option<Duration>,
}

/// Outcome of [`Balancer::pick`].
pub enum Pick {
    /// Use `candidates[index]`. The connection slot is held until `slot` drops.
    Chosen { index: usize, slot: Slot },
    /// Every candidate has already been tried for this connection.
    AllTried,
    /// Every untried candidate is at its connection limit.
    AtCapacity,
}

/// A reserved connection slot on one backend; released on drop.
pub struct Slot {
    balancer: Arc<Balancer>,
    node_id: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut inner = self.balancer.inner.lock().unwrap();
        if let Some(count) = inner.active.get_mut(&self.node_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                inner.active.remove(&self.node_id);
            }
        }
    }
}

#[derive(Debug, Default)]
struct OutlierState {
    consecutive_failures: u32,
    /// Ejections since the last success; drives the backoff.
    ejections: u32,
    ejected_until: Option<Instant>,
}

impl OutlierState {
    fn is_ejected(&self, now: Instant) -> bool {
        self.ejected_until.is_some_and(|until| now < until)
    }
}

#[derive(Default)]
struct Inner {
    /// Smooth weighted round-robin running weight, keyed by node id.
    current: HashMap<String, i64>,
    outliers: HashMap<String, OutlierState>,
    /// Connections currently open to each node (only nodes with any).
    active: HashMap<String, u32>,
}

/// Load balancer state for a single service's proxy.
#[derive(Default)]
pub struct Balancer {
    inner: Mutex<Inner>,
}

impl Balancer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pick a backend from `candidates`, skipping node ids in `tried`, and
    /// reserve a connection slot on it.
    ///
    /// Nodes at their connection limit are never picked. Among the rest,
    /// drained (weight 0) nodes are skipped unless every remaining node is
    /// drained, in which case they share load equally, and ejected nodes are
    /// skipped unless every remaining node is ejected.
    pub fn pick(self: &Arc<Self>, candidates: &[Backend], tried: &[String], now: Instant) -> Pick {
        let mut inner = self.inner.lock().unwrap();

        // Forget nodes no longer hosting the service so the maps stay bounded.
        inner.current.retain(|id, _| candidates.iter().any(|b| &b.node.id == id));
        inner.outliers.retain(|id, _| candidates.iter().any(|b| &b.node.id == id));

        let untried: Vec<usize> = (0..candidates.len())
            .filter(|&i| !tried.contains(&candidates[i].node.id))
            .collect();
        if untried.is_empty() {
            return Pick::AllTried;
        }

        let open: Vec<usize> = untried
            .into_iter()
            .filter(|&i| {
                let active = inner.active.get(&candidates[i].node.id).copied().unwrap_or(0);
                candidates[i].max_connections.is_none_or(|max| active < max)
            })
            .collect();
        if open.is_empty() {
            return Pick::AtCapacity;
        }

        let undrained = fail_open(&open, |i| candidates[i].weight > 0);
        let eligible = fail_open(&undrained, |i| {
            !inner
                .outliers
                .get(&candidates[i].node.id)
                .is_some_and(|o| o.is_ejected(now))
        });

        // Smooth weighted round-robin (as in nginx): every eligible node gains
        // its weight, the highest running weight wins and pays back the total.
        // This interleaves picks rather than sending runs to one node.
        // Only reachable with weight 0 when every eligible node is drained.
        let weight_of = |i: usize| i64::from(candidates[i].weight.max(1));
        let total: i64 = eligible.iter().map(|&i| weight_of(i)).sum();

        let mut best: Option<(usize, i64)> = None;
        for &i in &eligible {
            let cur = inner.current.entry(candidates[i].node.id.clone()).or_insert(0);
            *cur += weight_of(i);
            if best.is_none_or(|(_, b)| *cur > b) {
                best = Some((i, *cur));
            }
        }

        let (chosen, _) = best.expect("eligible is non-empty");
        let node_id = candidates[chosen].node.id.clone();
        *inner.current.get_mut(&node_id).unwrap() -= total;
        *inner.active.entry(node_id.clone()).or_insert(0) += 1;
        Pick::Chosen {
            index: chosen,
            slot: Slot { balancer: Arc::clone(self), node_id },
        }
    }

    /// Record that a tunnel to `node_id` was established.
    pub fn record_success(&self, node_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(state) = inner.outliers.get_mut(node_id) {
            *state = OutlierState::default();
        }
    }

    /// Record a failed tunnel setup to `node_id`.
    ///
    /// Returns the ejection duration if this failure ejected the node.
    pub fn record_failure(&self, node_id: &str, now: Instant) -> Option<Duration> {
        let mut inner = self.inner.lock().unwrap();
        let state = inner.outliers.entry(node_id.to_string()).or_default();
        if state.is_ejected(now) {
            // Already out of rotation (e.g. a fail-open pick); don't extend.
            return None;
        }
        state.consecutive_failures += 1;
        if state.consecutive_failures < EJECT_AFTER_FAILURES {
            return None;
        }

        state.consecutive_failures = 0;
        state.ejections += 1;
        let factor = 1u32 << (state.ejections - 1).min(16);
        let duration = BASE_EJECTION.saturating_mul(factor).min(MAX_EJECTION);
        state.ejected_until = Some(now + duration);
        Some(duration)
    }
}

/// Indices in `set` satisfying `keep`, or all of `set` if none do.
fn fail_open(set: &[usize], keep: impl Fn(usize) -> bool) -> Vec<usize> {
    let kept: Vec<usize> = set.iter().copied().filter(|&i| keep(i)).collect();
    if kept.is_empty() {
        set.to_vec()
    } else {
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(id: &str, weight: u32) -> Backend {
        Backend {
            node: NodeInfo { id: id.into(), addr: String::new(), relay: None, last_seen: None, health: Default::default() },
            weight,
            max_connections: None,
            request_timeout: None,
        }
    }

    fn limited(id: &str, max: u32) -> Backend {
        Backend { max_connections: Some(max), ..backend(id, 1) }
    }

    /// Pick once, returning the chosen node id and its (held) slot.
    fn pick_one(b: &Arc<Balancer>, backends: &[Backend], tried: &[String], now: Instant) -> Option<(String, Slot)> {
        match b.pick(backends, tried, now) {
            Pick::Chosen { index, slot } => Some((backends[index].node.id.clone(), slot)),
            Pick::AllTried | Pick::AtCapacity => None,
        }
    }

    /// Pick `n` times with no retries, releasing each slot immediately, and
    /// return the chosen node ids.
    fn picks(b: &Arc<Balancer>, backends: &[Backend], n: usize, now: Instant) -> Vec<String> {
        (0..n)
            .map(|_| pick_one(b, backends, &[], now).expect("a pick").0)
            .collect()
    }

    fn count(ids: &[String], id: &str) -> usize {
        ids.iter().filter(|x| *x == id).count()
    }

    #[test]
    fn equal_weights_round_robin() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 1), backend("b", 1), backend("c", 1)];
        assert_eq!(picks(&b, &backends, 6, Instant::now()), ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn weights_split_traffic_proportionally_and_interleave() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 5), backend("b", 1), backend("c", 1)];
        let got = picks(&b, &backends, 7, Instant::now());
        // Smooth WRR: the nginx reference sequence for weights 5/1/1.
        assert_eq!(got, ["a", "a", "b", "a", "c", "a", "a"]);

        let got = picks(&b, &backends, 700, Instant::now());
        assert_eq!(count(&got, "a"), 500);
        assert_eq!(count(&got, "b"), 100);
        assert_eq!(count(&got, "c"), 100);
    }

    #[test]
    fn canary_split() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("stable", 9), backend("canary", 1)];
        let got = picks(&b, &backends, 1000, Instant::now());
        assert_eq!(count(&got, "canary"), 100);
    }

    #[test]
    fn zero_weight_drains_node() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 1), backend("b", 0)];
        let got = picks(&b, &backends, 10, Instant::now());
        assert_eq!(count(&got, "b"), 0);
    }

    #[test]
    fn all_drained_fails_open_to_equal_split() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 0), backend("b", 0)];
        assert_eq!(picks(&b, &backends, 4, Instant::now()), ["a", "b", "a", "b"]);
    }

    #[test]
    fn drained_node_used_as_retry_of_last_resort() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 1), backend("b", 0)];
        let (id, _) = pick_one(&b, &backends, &["a".into()], Instant::now()).unwrap();
        assert_eq!(id, "b");
    }

    #[test]
    fn retry_skips_tried_nodes_and_ends_when_exhausted() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 10), backend("b", 1)];
        let now = Instant::now();
        let (id, _) = pick_one(&b, &backends, &["a".into()], now).unwrap();
        assert_eq!(id, "b");
        assert!(matches!(b.pick(&backends, &["a".into(), "b".into()], now), Pick::AllTried));
    }

    #[test]
    fn ejects_after_consecutive_failures() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 1), backend("b", 1)];
        let now = Instant::now();

        assert_eq!(b.record_failure("a", now), None);
        assert_eq!(b.record_failure("a", now), None);
        assert_eq!(b.record_failure("a", now), Some(BASE_EJECTION));

        let got = picks(&b, &backends, 5, now);
        assert_eq!(count(&got, "a"), 0);

        // Back in rotation once the ejection expires.
        let later = now + BASE_EJECTION;
        assert!(picks(&b, &backends, 4, later).contains(&"a".to_string()));
    }

    #[test]
    fn success_resets_failure_count() {
        let b = Arc::new(Balancer::new());
        let now = Instant::now();
        b.record_failure("a", now);
        b.record_failure("a", now);
        b.record_success("a");
        assert_eq!(b.record_failure("a", now), None);
        assert_eq!(b.record_failure("a", now), None);
        assert!(b.record_failure("a", now).is_some());
    }

    #[test]
    fn repeated_ejections_back_off_up_to_cap() {
        let b = Arc::new(Balancer::new());
        let mut now = Instant::now();
        let mut durations = Vec::new();
        for _ in 0..6 {
            b.record_failure("a", now);
            b.record_failure("a", now);
            let d = b.record_failure("a", now).expect("ejected");
            durations.push(d.as_secs());
            now += d;
        }
        assert_eq!(durations, [10, 20, 40, 80, 120, 120]);

        // A success resets the backoff.
        b.record_success("a");
        b.record_failure("a", now);
        b.record_failure("a", now);
        assert_eq!(b.record_failure("a", now), Some(BASE_EJECTION));
    }

    #[test]
    fn failures_while_ejected_do_not_extend_ejection() {
        let b = Arc::new(Balancer::new());
        let now = Instant::now();
        for _ in 0..3 {
            b.record_failure("a", now);
        }
        for _ in 0..10 {
            assert_eq!(b.record_failure("a", now), None);
        }
        let backends = [backend("a", 1)];
        // Single node, ejected: fail-open still returns it.
        assert_eq!(pick_one(&b, &backends, &[], now).unwrap().0, "a");
        // And it is back in normal rotation exactly when the first ejection ends.
        let backends = [backend("a", 1), backend("b", 1)];
        assert!(picks(&b, &backends, 2, now + BASE_EJECTION).contains(&"a".to_string()));
    }

    #[test]
    fn all_ejected_fails_open() {
        let b = Arc::new(Balancer::new());
        let backends = [backend("a", 1), backend("b", 1)];
        let now = Instant::now();
        for id in ["a", "b"] {
            for _ in 0..EJECT_AFTER_FAILURES {
                b.record_failure(id, now);
            }
        }
        assert_eq!(picks(&b, &backends, 2, now), ["a", "b"]);
    }

    #[test]
    fn forgets_state_for_removed_nodes() {
        let b = Arc::new(Balancer::new());
        let now = Instant::now();
        for _ in 0..EJECT_AFTER_FAILURES {
            b.record_failure("gone", now);
        }
        let _ = b.pick(&[backend("a", 1)], &[], now);
        let inner = b.inner.lock().unwrap();
        assert!(!inner.outliers.contains_key("gone"));
        assert!(!inner.current.contains_key("gone"));
    }

    #[test]
    fn connection_limit_is_a_hard_cap() {
        let b = Arc::new(Balancer::new());
        let backends = [limited("a", 2)];
        let now = Instant::now();
        let s1 = pick_one(&b, &backends, &[], now).unwrap();
        let _s2 = pick_one(&b, &backends, &[], now).unwrap();
        assert!(matches!(b.pick(&backends, &[], now), Pick::AtCapacity));
        // Releasing a slot frees capacity.
        drop(s1);
        assert!(pick_one(&b, &backends, &[], now).is_some());
    }

    #[test]
    fn full_backend_is_skipped_in_favour_of_others() {
        let b = Arc::new(Balancer::new());
        let backends = [limited("a", 1), backend("b", 1)];
        let now = Instant::now();
        let (first, _held) = pick_one(&b, &backends, &[], now).unwrap();
        assert_eq!(first, "a");
        // a is full while its slot is held, so even its round-robin turn goes to b.
        for _ in 0..4 {
            assert_eq!(pick_one(&b, &backends, &[], now).unwrap().0, "b");
        }
    }

    #[test]
    fn capacity_is_not_fail_open_even_when_others_are_ejected() {
        let b = Arc::new(Balancer::new());
        let backends = [limited("a", 1), backend("b", 1)];
        let now = Instant::now();
        for _ in 0..EJECT_AFTER_FAILURES {
            b.record_failure("b", now);
        }
        let _held = pick_one(&b, &backends, &[], now).unwrap();
        // a is full and b is ejected: the ejection filter fails open, the
        // capacity limit does not.
        assert_eq!(pick_one(&b, &backends, &[], now).unwrap().0, "b");
        let backends = [limited("a", 1)];
        assert!(matches!(b.pick(&backends, &[], now), Pick::AtCapacity));
    }

    #[test]
    fn slots_survive_node_leaving_and_rejoining() {
        let b = Arc::new(Balancer::new());
        let now = Instant::now();
        let (_, held) = pick_one(&b, &[limited("a", 1)], &[], now).unwrap();
        // a disappears from the candidate set while its connection is open...
        let _ = b.pick(&[backend("b", 1)], &[], now);
        // ...and comes back: its open connection still counts.
        assert!(matches!(b.pick(&[limited("a", 1)], &[], now), Pick::AtCapacity));
        drop(held);
        assert!(b.inner.lock().unwrap().active.is_empty());
    }
}
