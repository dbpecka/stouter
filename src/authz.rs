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
use std::time::{Duration, Instant};

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
            if !state.tunnel_nonces.lock().unwrap().note(&signer, signed.nonce) {
                return Err(format!("tunnel request to '{service}' from '{signer}' was already used"));
            }
            signer
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
    metrics::counter!("stouter_tunnel_denied_total", "service" => service.to_string()).increment(1);
    tracing::debug!("refusing a tunnel to '{service}' from '{who}' (policy)");
    Err(format!("node '{who}' may not reach service '{service}' (policy)"))
}

/// How long a signed request's nonce is remembered: past the time its
/// timestamp is accepted for (30 seconds either side).
const NONCE_TTL: Duration = Duration::from_secs(70);

/// Most nonces remembered. Each comes from a request a cluster member
/// signed, so only a member flooding its own requests fills it; past it,
/// signed requests are refused until older nonces expire.
const MAX_NONCES: usize = 100_000;

/// Recently seen nonces of signed tunnel requests, by signer.
pub struct NonceCache {
    seen: HashMap<(String, u64), Instant>,
    pruned: Instant,
}

impl Default for NonceCache {
    fn default() -> Self {
        Self { seen: HashMap::new(), pruned: Instant::now() }
    }
}

impl NonceCache {
    /// Remember `signer`'s `nonce`; false if it was already seen (a replay)
    /// or there is no room.
    fn note(&mut self, signer: &str, nonce: u64) -> bool {
        let now = Instant::now();
        if now.duration_since(self.pruned) >= Duration::from_secs(1) || self.seen.len() >= MAX_NONCES {
            self.seen.retain(|_, at| now.duration_since(*at) < NONCE_TTL);
            self.pruned = now;
        }
        if self.seen.len() >= MAX_NONCES {
            return false;
        }
        self.seen.insert((signer.to_string(), nonce), now).is_none()
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
        let mut cache = NonceCache::default();
        assert!(cache.note("a", 1));
        assert!(!cache.note("a", 1));
        assert!(cache.note("b", 1), "nonces are per signer");
        // Expired ones are forgotten.
        cache.seen.values_mut().for_each(|at| *at -= NONCE_TTL);
        cache.pruned -= Duration::from_secs(2);
        assert!(cache.note("a", 1));
        // Full: refused rather than forgetting a fresh one.
        cache.seen.clear();
        for i in 0..MAX_NONCES as u64 {
            cache.seen.insert(("f".into(), i), Instant::now());
        }
        assert!(!cache.note("a", 2));
    }
}
