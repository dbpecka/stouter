//! Active health checks for the services this node hosts.
//!
//! Every [`CHECK_INTERVAL`] the node opens a TCP connection to each local
//! service's `nodePort`. Results feed [`HealthTracker`], and when the set of
//! unhealthy services changes the new report is stored in
//! `SharedState::local_health` and pushed to peers immediately. Subscribers
//! use it to skip a dead backend before any client connection reaches it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::ServiceHealth;
use crate::gossip;
use crate::state::SharedState;

/// How often each local service is probed.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// A probe that has not connected within this long counts as a failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Consecutive failed probes before a service is reported unhealthy. One
/// success marks it healthy again.
const UNHEALTHY_AFTER: u32 = 2;

/// Health-check state machine for a set of services, independent of I/O.
///
/// Services start healthy so a node never reports a backend down before it
/// has actually probed it.
#[derive(Default)]
pub struct HealthTracker {
    consecutive_failures: HashMap<String, u32>,
}

impl HealthTracker {
    /// Record a probe result. Returns `true` if the service's health flipped.
    pub fn record(&mut self, service: &str, ok: bool) -> bool {
        let failures = self.consecutive_failures.entry(service.to_string()).or_insert(0);
        let was_unhealthy = *failures >= UNHEALTHY_AFTER;
        *failures = if ok { 0 } else { failures.saturating_add(1) };
        was_unhealthy != (*failures >= UNHEALTHY_AFTER)
    }

    /// Forget services no longer hosted here. Returns `true` if an unhealthy
    /// service was dropped (so the report changed).
    pub fn retain(&mut self, hosted: &[String]) -> bool {
        let before = self.unhealthy();
        self.consecutive_failures.retain(|s, _| hosted.contains(s));
        before != self.unhealthy()
    }

    /// Sorted names of services currently considered unhealthy.
    pub fn unhealthy(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .consecutive_failures
            .iter()
            .filter(|(_, &f)| f >= UNHEALTHY_AFTER)
            .map(|(s, _)| s.clone())
            .collect();
        names.sort();
        names
    }
}

/// Next report version: wall-clock milliseconds, forced strictly above the
/// previous version so ordering holds even if the clock steps backwards.
fn next_version(prev: u64) -> u64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now_ms.max(prev + 1)
}

/// Background task: probe every service hosted on this node until shutdown.
pub async fn run_health_checks(state: Arc<SharedState>) {
    let mut tracker = HealthTracker::default();
    let mut interval = time::interval(CHECK_INTERVAL);
    // Always publish after the first round: peers may still hold a report
    // from before a restart (e.g. "unhealthy"), and only a newer version
    // replaces it, even if this round finds everything healthy.
    let mut first_round = true;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }

        let hosted: Vec<(String, u16)> = state
            .service_index
            .load()
            .iter()
            .filter(|((node_id, _), _)| *node_id == state.config.node_id)
            .map(|((_, name), svc)| (name.clone(), svc.node_port))
            .collect();

        let names: Vec<String> = hosted.iter().map(|(n, _)| n.clone()).collect();
        let mut changed = tracker.retain(&names) || std::mem::take(&mut first_round);

        let mut probes = JoinSet::new();
        for (name, port) in hosted {
            probes.spawn(async move {
                let ok = probe(port).await;
                (name, port, ok)
            });
        }
        while let Some(Ok((name, port, ok))) = probes.join_next().await {
            if !ok {
                debug!("health check failed for {name} on 127.0.0.1:{port}");
            }
            if tracker.record(&name, ok) {
                changed = true;
                if ok {
                    info!("service {name} is healthy again");
                } else {
                    warn!("service {name} is unhealthy: 127.0.0.1:{port} failed {UNHEALTHY_AFTER} checks in a row");
                }
            }
        }

        if changed {
            let prev = state.local_health.load().version;
            state.local_health.store(Arc::new(ServiceHealth {
                version: next_version(prev),
                unhealthy: tracker.unhealthy(),
            }));
            // Push now rather than waiting for the next sync round.
            let state = state.clone();
            tokio::spawn(async move { gossip::broadcast_sync(&state).await });
        }
    }
}

/// TCP-connect probe of a local backend.
async fn probe(port: u16) -> bool {
    matches!(
        time::timeout(PROBE_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unhealthy_after_consecutive_failures_and_recovers_on_success() {
        let mut t = HealthTracker::default();
        assert!(!t.record("api", false));
        assert!(t.unhealthy().is_empty());
        assert!(t.record("api", false));
        assert_eq!(t.unhealthy(), ["api"]);
        assert!(!t.record("api", false));
        assert!(t.record("api", true));
        assert!(t.unhealthy().is_empty());
        assert!(!t.record("api", true));
    }

    #[test]
    fn single_failure_between_successes_does_not_flip() {
        let mut t = HealthTracker::default();
        for ok in [true, false, true, false, true] {
            assert!(!t.record("api", ok));
        }
        assert!(t.unhealthy().is_empty());
    }

    #[test]
    fn retain_reports_change_only_when_unhealthy_service_dropped() {
        let mut t = HealthTracker::default();
        t.record("a", false);
        t.record("a", false);
        t.record("b", true);
        assert!(!t.retain(&["a".into()]));
        assert!(t.retain(&[]));
        assert!(t.unhealthy().is_empty());
    }

    #[test]
    fn versions_strictly_increase() {
        let v1 = next_version(0);
        assert!(v1 > 0);
        assert!(next_version(v1) > v1);
        assert_eq!(next_version(u64::MAX - 1), u64::MAX);
    }

    #[tokio::test]
    async fn probe_detects_listening_and_closed_ports() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe(port).await);
        drop(listener);
        assert!(!probe(port).await);
    }
}
