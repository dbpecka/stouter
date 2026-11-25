//! Service authorization: which nodes may open tunnels to which services.
//!
//! A service's policy (the config's `policies`) lists the nodes that may
//! reach it; a service with no policy is open to the whole cluster, as
//! before. The node hosting the service decides, as only it can: a caller
//! could skip a check of its own. The nodes hosting a service may always
//! reach it.
//!
//! Who is asking is the certificate at the other end of the connection
//! when the caller connected directly (a new, pooled or mux connection). A
//! request for a NAT'd node arrives on the reverse connection that node
//! keeps at a relay, whose certificate says only who the relay is; so
//! callers sign such requests (`crate::signing::sign_tunnel_request`), and
//! the hosting node checks the signature. A relay can pass a request on but
//! not make one up for another node, and a request passed on twice is
//! refused. (A relay still carries a tunnel's bytes, as it always has: it
//! can tamper with or cut off the tunnels that go through it.)

use std::collections::{HashMap, HashSet};

use crate::config::{DynamicConfig, SignedCaller};
use crate::state::SharedState;

/// Service name -> the nodes that may reach it (see [`build`]).
pub type Policies = HashMap<String, HashSet<String>>;

/// The policies in `dc`, by service.
pub fn build(dc: &DynamicConfig) -> Policies {
    dc.policies.iter().map(|p| (p.service.clone(), p.allow.iter().cloned().collect())).collect()
}

/// Whether node `caller` may open tunnels to `service`: the service has no
/// policy, its policy lists the node, or the node hosts an instance of it
/// itself (instances of a service may reach one another).
pub fn may_reach(state: &SharedState, service: &str, caller: &str) -> bool {
    state.policies.load().get(service).is_none_or(|allow| allow.contains(caller))
        || state.service_index.load().contains_key(&(caller.to_string(), service.to_string()))
}

/// How a tunnel request reached this node.
#[derive(Clone, Copy, Debug)]
pub enum Via<'a> {
    /// From the caller itself: the Common Name of the certificate at the
    /// other end of the connection.
    Direct(Option<&'a str>),
    /// Through a relay, on a reverse connection this node keeps there.
    Relay,
}

/// Whether a tunnel request for our `service` may go ahead; if not, why
/// (to send back to the caller). `caller` is the request's signature, if
/// it has one; `timestamp_ms` its timestamp (already checked for skew).
pub fn authorize(
    state: &SharedState,
    service: &str,
    timestamp_ms: u64,
    caller: Option<&SignedCaller>,
    via: Via<'_>,
) -> Result<(), String> {
    if !state.policies.load().contains_key(service) {
        return Ok(());
    }
    let own = state.config.node_id.as_str();
    let who = match (caller, via) {
        (Some(signed), via) => {
            let tls = state.tls.as_deref().ok_or("no TLS context to check the caller's signature with")?;
            let signer = crate::signing::verify_tunnel_request(signed, own, service, timestamp_ms, tls)
                .map_err(|e| format!("tunnel request to '{service}': the caller's signature doesn't check out: {e:#}"))?;
            // Directly, the caller is the node at the other end: a request
            // someone else signed is one passed on, which only relays do.
            if let Via::Direct(Some(peer)) = via {
                if peer != signer {
                    return Err(format!("tunnel request to '{service}' signed by '{signer}' but sent by '{peer}'"));
                }
            }
            // Its nonce is noted (against replays) only once the caller is
            // known to be allowed, below: a refused caller would otherwise
            // fill the cache, locking allowed ones out.
            return if signer == own || may_reach(state, service, &signer) {
                if state.tunnel_nonces.lock().unwrap().note(&signer, signed.nonce, timestamp_ms) {
                    Ok(())
                } else {
                    Err(format!("tunnel request to '{service}' from '{signer}' was already used (or it sent too many)"))
                }
            } else {
                refuse(service, &signer)
            };
        }
        (None, Via::Direct(Some(peer))) => peer.to_string(),
        (None, Via::Direct(None)) => return Err("the caller's certificate names no node".into()),
        (None, Via::Relay) => {
            return Err(format!(
                "service '{service}' takes only tunnel requests the caller signed through a relay, and this one \
                 isn't signed (from an older stouter?)"
            ))
        }
    };
    if who == own || may_reach(state, service, &who) {
        return Ok(());
    }
    refuse(service, &who)
}

/// Refuse `who` a tunnel to `service` (policy), counted.
fn refuse(service: &str, who: &str) -> Result<(), String> {
    metrics::counter!("stouter_tunnel_denied_total", "service" => service.to_string()).increment(1);
    tracing::debug!("refusing a tunnel to '{service}' from '{who}' (policy)");
    Err(format!("node '{who}' may not reach service '{service}' (policy)"))
}

/// How long after its timestamp a signed request's nonce is remembered:
/// for as long as the request could still pass the freshness check (its
/// timestamp within 30 seconds of now), and a second more. By the wall
/// clock, as that check is: a clock stepped back can't bring back a
/// request whose nonce was let go.
const NONCE_KEPT_MS: u64 = 31_000;

/// Most nonces remembered. Each comes from a request a cluster member
/// signed (and this node verified), so only an allowed member flooding its
/// own requests fills it; past it, signed requests are refused until older
/// nonces expire. About 32 bytes each.
const MAX_NONCES: usize = 500_000;

/// Most nonces remembered for one signer: a tenth of the cache, so one
/// allowed caller flooding its own requests can't crowd out the others.
/// Requests are fresh for about a minute, so this is some 800 new tunnels a
/// second from one caller to this node.
const MAX_NONCES_PER_SIGNER: usize = MAX_NONCES / 10;

/// How often every signer's expired nonces are let go (each signer's are
/// also when it sends its next request).
const PRUNE_EVERY_MS: u64 = 1_000;

/// Recently seen nonces of signed tunnel requests, by signer.
#[derive(Default)]
pub struct NonceCache {
    signers: HashMap<std::sync::Arc<str>, SignerNonces>,
    /// Nonces held, across signers.
    total: usize,
    /// When every signer's were last pruned (Unix ms).
    pruned_ms: u64,
}

/// One signer's remembered nonces.
#[derive(Default)]
struct SignerNonces {
    seen: HashSet<u64>,
    /// The same nonces, with when each may be let go (Unix ms), oldest
    /// first, near enough: a request delayed in transit may expire before
    /// one ahead of it, and then waits for it (kept a little longer, never
    /// less).
    expiry: std::collections::VecDeque<(u64, u64)>,
}

impl SignerNonces {
    /// Let go of nonces expired by `now_ms`; how many.
    fn prune(&mut self, now_ms: u64) -> usize {
        let mut gone = 0;
        while let Some(&(until, nonce)) = self.expiry.front() {
            if until >= now_ms {
                break;
            }
            self.expiry.pop_front();
            self.seen.remove(&nonce);
            gone += 1;
        }
        gone
    }
}

impl NonceCache {
    /// Remember `signer`'s `nonce`, from a request signed with timestamp
    /// `timestamp_ms`; false if it was already seen (a replay) or there is
    /// no room (for anyone, or for this signer).
    pub fn note(&mut self, signer: &str, nonce: u64, timestamp_ms: u64) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        self.note_at(signer, nonce, timestamp_ms, now_ms)
    }

    fn note_at(&mut self, signer: &str, nonce: u64, timestamp_ms: u64, now_ms: u64) -> bool {
        // Every signer's now and then (one that went quiet holds nothing
        // for long); this signer's every time. Never a scan of the lot.
        if now_ms.abs_diff(self.pruned_ms) >= PRUNE_EVERY_MS || self.total >= MAX_NONCES {
            let mut gone = 0;
            self.signers.retain(|_, nonces| {
                gone += nonces.prune(now_ms);
                !nonces.seen.is_empty()
            });
            self.total -= gone;
            self.pruned_ms = now_ms;
        }
        if let Some(nonces) = self.signers.get_mut(signer) {
            self.total -= nonces.prune(now_ms);
            if nonces.seen.contains(&nonce) {
                return false;
            }
        }
        let mine = self.signers.get(signer).map_or(0, |nonces| nonces.seen.len());
        if self.total >= MAX_NONCES || mine >= MAX_NONCES_PER_SIGNER {
            return false;
        }
        let nonces = self.signers.entry(std::sync::Arc::from(signer)).or_default();
        nonces.seen.insert(nonce);
        nonces.expiry.push_back((timestamp_ms.saturating_add(NONCE_KEPT_MS), nonce));
        self.total += 1;
        true
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.total == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::signing::{sign_tunnel_request, Signer};
    use crate::state::test_support::tls_state_with;

    fn now_ms() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
    }

    /// Node "x", hosting "db" (with a replica on "y") and "open"; only "a"
    /// may reach "db". Signers for "a", "b" and "y" from the same CA.
    fn setup() -> (std::path::PathBuf, std::sync::Arc<SharedState>, impl Fn(&str) -> Signer) {
        let dir = std::env::temp_dir().join(format!("stouter_authz_{}_{}", std::process::id(), rand::random::<u64>()));
        let dc: DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "serviceGroups": [{"name": "g", "services": [
                {"name": "db", "nodeId": "x", "nodePort": 5432},
                {"name": "db", "nodeId": "y", "nodePort": 5432},
                {"name": "open", "nodeId": "x", "nodePort": 80},
            ]}],
            "policies": [{"service": "db", "allow": ["a"]}],
        }))
        .unwrap();
        let x = tls_state_with(&dir, Config { node_id: "x".into(), dynamic_config: dc, ..Config::default() }, String::new());
        let ca = dir.clone();
        let signer = move |id: &str| Signer::from_material(&crate::pki::load_or_create(ca.to_str().unwrap(), id).unwrap()).unwrap();
        (dir, x, signer)
    }

    #[test]
    fn only_the_nodes_a_policy_lists_reach_its_service() {
        let (dir, x, signer) = setup();
        let ts = now_ms();
        let check = |service: &str, caller: Option<&SignedCaller>, via: Via<'_>| authorize(&x, service, ts, caller, via);

        // No policy: anyone, any way.
        assert!(check("open", None, Via::Direct(Some("b"))).is_ok());
        assert!(check("open", None, Via::Relay).is_ok());

        // Directly: the certificate at the other end decides.
        assert!(check("db", None, Via::Direct(Some("a"))).is_ok());
        let err = check("db", None, Via::Direct(Some("b"))).unwrap_err();
        assert!(err.contains("'b' may not reach service 'db'"), "{err}");
        assert!(check("db", None, Via::Direct(Some("y"))).is_ok(), "a node hosting an instance");
        assert!(check("db", None, Via::Direct(Some("x"))).is_ok(), "the node itself");

        // Through a relay: only a signature says who asks.
        let err = check("db", None, Via::Relay).unwrap_err();
        assert!(err.contains("isn't signed"), "{err}");
        let by_a = sign_tunnel_request(&signer("a"), "x", "db", ts).unwrap();
        assert!(check("db", Some(&by_a), Via::Relay).is_ok());
        let err = check("db", Some(&by_a), Via::Relay).unwrap_err();
        assert!(err.contains("already used"), "a request passed on twice: {err}");
        let by_b = sign_tunnel_request(&signer("b"), "x", "db", ts).unwrap();
        assert!(check("db", Some(&by_b), Via::Relay).unwrap_err().contains("'b' may not reach"));

        // A signature for another service, node or time doesn't carry over.
        for (node, service, at) in [("x", "open", ts), ("z", "db", ts), ("x", "db", ts + 1)] {
            let other = sign_tunnel_request(&signer("a"), node, service, at).unwrap();
            let err = check("db", Some(&other), Via::Relay).unwrap_err();
            assert!(err.contains("signature doesn't check out"), "{node}/{service}/{at}: {err}");
        }

        // Directly, a request someone else signed is one passed on.
        let by_a = sign_tunnel_request(&signer("a"), "x", "db", ts).unwrap();
        let err = check("db", Some(&by_a), Via::Direct(Some("b"))).unwrap_err();
        assert!(err.contains("signed by 'a' but sent by 'b'"), "{err}");

        // A revoked caller's signature counts for nothing.
        x.tls.as_ref().unwrap().revocations().set(&crate::config::Revoked { nodes: vec!["a".into()], certs: vec![] });
        let by_a = sign_tunnel_request(&signer("a"), "x", "db", ts).unwrap();
        assert!(check("db", Some(&by_a), Via::Relay).unwrap_err().contains("signature doesn't check out"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replays_are_remembered_only_so_long_and_so_many() {
        let now = 1_700_000_000_000;
        let mut cache = NonceCache::default();
        assert!(cache.note_at("a", 1, now, now));
        assert!(!cache.note_at("a", 1, now, now));
        assert!(cache.note_at("b", 1, now, now), "nonces are per signer");
        // Kept while the request could still pass the freshness check
        // (within 30 s of its timestamp), let go after.
        assert!(!cache.note_at("a", 1, now, now + 30_000));
        assert!(cache.note_at("a", 1, now, now + NONCE_KEPT_MS + 1));
        // By the wall clock: a clock stepped back (by a minute, say) brings
        // nothing back. A request from before the step is still within 30 s
        // of the corrected clock, and its nonce is still held.
        let mut cache = NonceCache::default();
        assert!(cache.note_at("a", 7, now, now));
        assert!(!cache.note_at("a", 7, now, now + 70_000 - 60_000), "stepped back: still a replay");

        // Full: refused rather than forgetting a fresh one.
        let mut cache = NonceCache::default();
        for i in 0..MAX_NONCES as u64 {
            assert!(cache.note_at(&format!("s{}", i % 20), i, now, now));
        }
        assert!(!cache.note_at("a", 2, now, now));
        // Once they expire, there is room again, without a scan per call.
        assert!(cache.note_at("a", 2, now + 60_000, now + 60_000));
        assert_eq!(cache.total, 1);

        // One signer gets at most its share, and others still have room.
        let mut cache = NonceCache::default();
        for i in 0..MAX_NONCES_PER_SIGNER as u64 {
            assert!(cache.note_at("flood", i, now, now));
        }
        assert!(!cache.note_at("flood", u64::MAX, now, now), "its share is used up");
        assert!(cache.note_at("other", 1, now, now));
    }

    /// A busy allowed caller: hundreds of new tunnels a second, for minutes,
    /// all taken (the old cache refused past about 143 a second).
    #[test]
    fn a_busy_caller_is_not_refused() {
        let start = 1_700_000_000_000u64;
        let mut cache = NonceCache::default();
        let mut nonce = 0;
        for ms in (0..180_000).step_by(2) {
            nonce += 1;
            assert!(cache.note_at("busy", nonce, start + ms, start + ms), "refused at {ms} ms");
        }
        assert!(cache.total <= 16_000, "{}", cache.total);
    }

    /// A caller the policy refuses leaves no nonce behind: otherwise it could
    /// fill the cache and lock allowed callers out (through relays, where
    /// requests must be signed).
    #[test]
    fn refused_callers_use_up_no_room() {
        let (dir, x, signer) = setup();
        let b = signer("b");
        for _ in 0..1000 {
            let ts = now_ms();
            let req = sign_tunnel_request(&b, "x", "db", ts).unwrap();
            assert!(authorize(&x, "db", ts, Some(&req), Via::Relay).is_err());
        }
        assert!(x.tunnel_nonces.lock().unwrap().is_empty());
        let ts = now_ms();
        let req = sign_tunnel_request(&signer("a"), "x", "db", ts).unwrap();
        assert!(authorize(&x, "db", ts, Some(&req), Via::Relay).is_ok());
        assert!(authorize(&x, "db", ts, Some(&req), Via::Relay).is_err(), "a replay");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
