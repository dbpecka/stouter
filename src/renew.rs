//! Certificate renewal without restarts.
//!
//! Every node renews its certificate once a third of its lifetime is left,
//! and switches to the new one at once: new connections present it, and
//! its record and beats are signed with it. Connections already open keep
//! the one they began with. The CA node's `certLifetime` sets how long
//! certificates last (10 years by default), so they can be short-lived,
//! and a leaked key stops being good soon even if nobody revokes it.
//!
//! - The CA node (its `tlsDir` holds `ca.key`) issues its own new
//!   certificate. It lives in memory only, as its certificate always has:
//!   a new one is issued at each start.
//! - Any other node makes a new key and asks a node holding the CA's key
//!   for a certificate on it (a `CertRequest` with a CSR), over a
//!   connection made with its current certificate, which says whom the new
//!   one is for: a node can only renew its own, and a revoked certificate
//!   can't connect to ask. The new pair is written to `tlsDir`, for the
//!   next start.
//! - A certificate copied into `tlsDir` by hand (`node.crt` and
//!   `node.key`) is taken up the same way, without a restart.
//!
//! A node's certificate must be renewed before it expires: an expired one
//! can't connect to ask for another (and peers refuse it). Until a renewal
//! succeeds, it is tried again every check, with warnings as expiry nears.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tracing::{debug, info, warn};

use crate::gossip::messages::Message;
use crate::state::SharedState;
use crate::tls::MeshStream;

/// One renewal per node per this long, at most, at the CA node.
const MIN_RENEWAL_INTERVAL: Duration = Duration::from_secs(10);

/// Most nodes asked for a renewal in one attempt.
const MAX_ASKED: usize = 5;

/// Answer a node's `CertRequest`, at a node holding the CA's key.
pub async fn handle_cert_request(mut stream: MeshStream, csr: String, state: Arc<SharedState>) {
    let reply = match issue(&stream, &csr, &state) {
        Ok(cert) => Message::CertResponse { cert: Some(cert), error: None },
        Err(e) => Message::CertResponse { cert: None, error: Some(format!("{e:#}")) },
    };
    if let Err(e) = crate::gossip::send_message(&mut stream, &reply).await {
        debug!("certificate renewal: failed to send the reply: {e}");
    }
}

/// A renewed certificate (DER, hex) for the node at the other end of
/// `stream`, on the key in `csr` (DER, hex).
fn issue(stream: &MeshStream, csr: &str, state: &SharedState) -> Result<String> {
    let issuer = state.issuer.get().ok_or_else(|| anyhow!("this node doesn't hold the cluster CA's key"))?;
    let node = stream.peer_cn().ok_or_else(|| anyhow!("your certificate names no node"))?.to_string();
    {
        let mut renewals = state.renewals.lock().unwrap();
        let now = Instant::now();
        if renewals.get(&node).is_some_and(|at| now.duration_since(*at) < MIN_RENEWAL_INTERVAL) {
            bail!("renewed a moment ago; try again in {}s", MIN_RENEWAL_INTERVAL.as_secs());
        }
        renewals.retain(|_, at| now.duration_since(*at) < MIN_RENEWAL_INTERVAL);
        renewals.insert(node.clone(), now);
    }
    let csr = unhex(csr).ok_or_else(|| anyhow!("the certificate request isn't hex"))?;
    let cert = issuer.issue_for_csr(&node, &csr)?;
    let until = crate::pki::validity(cert.as_ref()).map_or_else(String::new, |(_, until)| {
        format!(" (valid until {})", crate::history::format_time(until.max(0) as u64 * 1000))
    });
    info!("issued a renewed certificate for '{node}'{until}");
    Ok(hex(cert.as_ref()))
}

/// Background task: renew this node's certificate before it expires, and
/// take up a certificate copied into `tlsDir` by hand.
pub async fn run_renewal_loop(state: Arc<SharedState>) {
    let mut failures: u32 = 0;
    loop {
        let wait = state.tls.as_ref().map_or(Duration::from_secs(3600), |tls| check_interval(&tls.own_cert()));
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = state.shutdown.cancelled() => return,
        }
        if let Err(e) = take_up_copied_cert(&state) {
            warn!("not using the certificate in tlsDir: {e:#}");
        }
        let Some(tls) = state.tls.as_ref() else { return };
        let cert = tls.own_cert();
        if !renewal_due(&cert, now_secs()) {
            failures = 0;
            continue;
        }
        match renew(&state).await {
            Ok(until) => {
                failures = 0;
                info!("renewed this node's certificate: valid until {}", crate::history::format_time(until.max(0) as u64 * 1000));
            }
            Err(e) => {
                failures += 1;
                let left = crate::pki::validity(cert.as_ref()).map_or(0, |(_, until)| until - now_secs());
                if left <= 0 {
                    tracing::error!(
                        "this node's certificate has expired, so peers refuse it and it can't renew: issue it a new one \
                         (`stouter cert issue`) and copy it into tlsDir ({e:#})"
                    );
                } else if failures.is_power_of_two() {
                    warn!("can't renew this node's certificate yet ({e:#}); it expires in {}", human(left));
                }
            }
        }
    }
}

/// How often to check: a tenth of the certificate's lifetime, between 5
/// seconds and an hour.
fn check_interval(cert: &CertificateDer<'_>) -> Duration {
    let lifetime = nominal_lifetime(cert).unwrap_or(3600);
    Duration::from_secs((lifetime / 10).clamp(5, 3600) as u64)
}

/// How long `cert` was issued for (seconds), not counting the allowance
/// for clock skew it starts with.
fn nominal_lifetime(cert: &CertificateDer<'_>) -> Option<i64> {
    let (from, until) = crate::pki::validity(cert.as_ref())?;
    Some((until - from - crate::pki::CLOCK_SKEW_ALLOWANCE.whole_seconds()).max(1))
}

/// Whether `cert` should be renewed at `now` (Unix seconds): a third or
/// less of its lifetime is left.
fn renewal_due(cert: &CertificateDer<'_>, now: i64) -> bool {
    match (crate::pki::validity(cert.as_ref()), nominal_lifetime(cert)) {
        (Some((_, until)), Some(lifetime)) => until - now <= lifetime / 3,
        _ => false,
    }
}

/// Renew this node's certificate now. Returns when the new one expires.
pub async fn renew(state: &Arc<SharedState>) -> Result<i64> {
    let id = state.config.node_id.clone();
    let (cert, key, persist) = match state.issuer.get() {
        Some(issuer) => {
            let (cert, key) = issuer.issue_new(&id)?;
            (cert, key, false)
        }
        None => {
            let (csr, key) = crate::pki::new_csr(&id)?;
            (ask_for_cert(state, &csr).await?, key, true)
        }
    };
    if crate::tls::cert_cn(&cert).as_deref() != Some(id.as_str()) {
        bail!("the renewed certificate isn't for '{id}'");
    }
    // Saved before it is used, so what is in use is what the next start
    // loads; a save that fails leaves the renewal due, to be tried again.
    if persist {
        if let (Some(dir), Some(tls)) = (state.config.tls_dir.as_deref(), state.tls.as_deref()) {
            tls.check_identity(&cert, &key)?;
            crate::pki::save_renewed(dir, &cert, &key)
                .with_context(|| format!("couldn't save the renewed certificate to {dir}"))?;
        }
    }
    switch_to(state, cert.clone(), &key)?;
    Ok(crate::pki::validity(cert.as_ref()).map_or(0, |(_, until)| until))
}

/// Ask nodes holding the CA's key (any of the peers we know, live ones
/// first, in random order; most hold none) for a certificate on `csr`.
async fn ask_for_cert(state: &Arc<SharedState>, csr: &[u8]) -> Result<CertificateDer<'static>> {
    let now = Instant::now();
    let mut peers: Vec<(bool, String, Option<String>)> = state
        .known_nodes
        .load()
        .values()
        .filter(|n| !n.addr.is_empty() && !n.addr.starts_with("0.0.0.0") && n.id != state.config.node_id)
        .map(|n| (crate::state::node_is_live(n, now), n.addr.clone(), Some(n.id.clone())))
        .collect();
    for addr in &state.bootstrap_peers {
        if !peers.iter().any(|(_, a, _)| a == addr) {
            peers.push((true, addr.clone(), None));
        }
    }
    use rand::seq::SliceRandom;
    peers.shuffle(&mut rand::rng());
    peers.sort_by_key(|(live, _, _)| !live);
    let request = Message::CertRequest { csr: hex(csr) };
    let mut errors = Vec::new();
    for (_, addr, expect) in peers.into_iter().take(MAX_ASKED) {
        let answer = async {
            let mut stream = state.connect_mesh(&addr, expect.as_deref()).await?;
            crate::gossip::send_message(&mut stream, &request).await?;
            crate::gossip::recv_message(&mut stream).await
        };
        match tokio::time::timeout(Duration::from_secs(15), answer).await {
            Ok(Ok(Message::CertResponse { cert: Some(cert), .. })) => {
                return unhex(&cert).map(CertificateDer::from).ok_or_else(|| anyhow!("{addr} sent a certificate that isn't hex"));
            }
            Ok(Ok(Message::CertResponse { error, .. })) => {
                errors.push(format!("{addr}: {}", error.unwrap_or_else(|| "no certificate".into())));
            }
            Ok(Ok(_)) => errors.push(format!("{addr}: unexpected reply")),
            Ok(Err(e)) => errors.push(format!("{addr}: {e:#}")),
            Err(_) => errors.push(format!("{addr}: no answer")),
        }
    }
    if errors.is_empty() {
        bail!("no peer known to ask");
    }
    bail!("no node holding the CA's key renewed it: {}", errors.join("; "))
}

/// Use `cert` and `key` from now on: for new connections, and for signing
/// this node's record (re-signed at once, at a newer version, so peers take
/// it) and beats.
pub fn switch_to(state: &SharedState, cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<()> {
    let tls = state.tls.as_deref().ok_or_else(|| anyhow!("no TLS context"))?;
    tls.rotate(cert.clone(), key)?;
    if let Some(signer) = state.signer.get() {
        signer.replace(cert, key)?;
    }
    *state.own_record.lock().unwrap() = None;
    *state.own_beat.lock().unwrap() = None;
    Ok(())
}

/// Switch to the certificate in `tlsDir` if it was replaced by hand (a node
/// that doesn't hold the CA's key; the CA node's lives in memory).
fn take_up_copied_cert(state: &SharedState) -> Result<()> {
    let (Some(dir), Some(tls)) = (state.config.tls_dir.as_deref(), state.tls.as_deref()) else { return Ok(()) };
    if state.issuer.get().is_some() {
        return Ok(());
    }
    let Ok(on_disk) = crate::pki::read_cert(&format!("{dir}/node.crt")) else { return Ok(()) };
    let current = tls.own_cert();
    if on_disk == current {
        return Ok(());
    }
    // Only a newer one: an older copy left behind isn't an upgrade.
    let newer = match (crate::pki::validity(on_disk.as_ref()), crate::pki::validity(current.as_ref())) {
        (Some((_, a)), Some((_, b))) => a > b,
        _ => false,
    };
    if !newer {
        return Ok(());
    }
    let material = crate::pki::load_existing(dir, &state.config.node_id)?;
    if crate::tls::cert_cn(&material.node_cert_der).as_deref() != Some(state.config.node_id.as_str()) {
        bail!("{dir}/node.crt isn't for '{}'", state.config.node_id);
    }
    switch_to(state, material.node_cert_der, &material.node_key)?;
    info!("now using the certificate copied into {dir}");
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// `secs` as days, hours, minutes or seconds.
fn human(secs: i64) -> String {
    match secs {
        s if s >= 2 * 86_400 => format!("{} days", s / 86_400),
        s if s >= 2 * 3600 => format!("{} hours", s / 3600),
        s if s >= 120 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::test_support::{mesh_pair, tls_state_with};

    /// A CA node "ca" listening (it holds the CA's key, in `dir/ca`), and
    /// node "n1" with a certificate issued for `lifetime` (in `dir/n1`, no
    /// CA key), knowing the CA node.
    async fn setup(lifetime: time::Duration) -> (std::path::PathBuf, Arc<SharedState>, Arc<SharedState>) {
        let dir = std::env::temp_dir().join(format!("stouter_renewal_{}_{}", std::process::id(), rand::random::<u64>()));
        let (ca_dir, n1_dir) = (dir.join("ca"), dir.join("n1"));
        let ca = tls_state_with(&ca_dir, Config { node_id: "ca".into(), ..Config::default() }, String::new());
        let _ = ca.issuer.set(crate::pki::Issuer::load(ca_dir.to_str().unwrap(), time::Duration::hours(1)).unwrap().unwrap());
        crate::pki::issue_cert_to_dir_with(ca_dir.to_str().unwrap(), "n1", n1_dir.to_str().unwrap(), lifetime).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = ca.clone();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = listener.accept().await else { return };
                let server = server.clone();
                tokio::spawn(async move {
                    if let Ok(mesh) = server.tls_accept(tcp).await {
                        crate::gossip::dispatch_connection(mesh, peer.to_string(), server).await;
                    }
                });
            }
        });
        let cfg = Config {
            node_id: "n1".into(),
            known_nodes: vec![addr],
            tls_dir: Some(n1_dir.to_str().unwrap().into()),
            ..Config::default()
        };
        let n1 = tls_state_with(&n1_dir, cfg, String::new());
        (dir, ca, n1)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_renews_from_the_ca_node_and_switches_at_once() {
        let (dir, ca, n1) = setup(time::Duration::minutes(3)).await;
        let n1_tls = n1.tls.as_ref().unwrap();
        let before = n1_tls.own_cert();
        // A connection made with the old certificate, before renewing.
        let (mut open, _ca_end) = mesh_pair(&n1, &ca).await;

        let until = renew(&n1).await.expect("renewed");
        let after = n1_tls.own_cert();
        assert_ne!(after, before, "a new certificate");
        assert_eq!(crate::tls::cert_cn(&after).as_deref(), Some("n1"), "for the same node");
        let (_, old_until) = crate::pki::validity(before.as_ref()).unwrap();
        assert!(until > old_until + 3000, "for the CA node's lifetime (an hour), not the old one's");

        // New connections present it, as client and as server.
        let (_, at_ca) = mesh_pair(&n1, &ca).await;
        assert_eq!(at_ca.peer_fingerprint(), Some(crate::tls::fingerprint(after.as_ref())));
        let (at_ca_client, _) = mesh_pair(&ca, &n1).await;
        assert_eq!(at_ca_client.peer_fingerprint(), Some(crate::tls::fingerprint(after.as_ref())));
        // The connection made before carries on.
        use tokio::io::AsyncWriteExt;
        open.write_all(b"still here").await.unwrap();

        // Its record is re-signed with it, at a newer version.
        let signer = n1.signer.get().unwrap();
        let record = crate::config::NodeRecord { id: "n1".into(), ..Default::default() };
        let signed = crate::signing::sign_node(&record, signer).unwrap();
        assert_eq!(signed.cert, after.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>());
        assert!(n1.own_record.lock().unwrap().is_none(), "the cached record is dropped, to be signed anew");

        // Saved for the next start.
        let n1_dir = dir.join("n1");
        assert_eq!(crate::pki::load_existing(n1_dir.to_str().unwrap(), "n1").unwrap().node_cert_der, after);

        // Not again at once.
        let err = renew(&n1).await.unwrap_err();
        assert!(format!("{err:#}").contains("renewed a moment ago"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_ca_node_renews_its_own_and_a_revoked_node_cant() {
        let (dir, ca, n1) = setup(time::Duration::minutes(3)).await;
        // The CA node issues its own, in memory.
        let before = ca.tls.as_ref().unwrap().own_cert();
        renew(&ca).await.unwrap();
        assert_ne!(ca.tls.as_ref().unwrap().own_cert(), before);

        // A revoked node can't connect to ask.
        ca.tls.as_ref().unwrap().revocations().set(&crate::config::Revoked { nodes: vec!["n1".into()], certs: vec![] });
        assert!(renew(&n1).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renewal_is_due_with_a_third_of_its_life_left() {
        let dir = std::env::temp_dir().join(format!("stouter_due_{}_{}", std::process::id(), rand::random::<u64>()));
        crate::pki::load_or_create(dir.to_str().unwrap(), "ca").unwrap();
        let issuer = crate::pki::Issuer::load(dir.to_str().unwrap(), time::Duration::seconds(300)).unwrap().unwrap();
        let (cert, _) = issuer.issue_new("n1").unwrap();
        let (_, until) = crate::pki::validity(cert.as_ref()).unwrap();
        assert!(!renewal_due(&cert, until - 200), "two thirds left");
        assert!(renewal_due(&cert, until - 99), "a third left");
        assert!(renewal_due(&cert, until + 1), "expired");
        assert_eq!(check_interval(&cert), Duration::from_secs(30), "a tenth of its life");
        let ten_years = crate::pki::Issuer::load(dir.to_str().unwrap(), crate::pki::DEFAULT_CERT_LIFETIME).unwrap().unwrap();
        assert_eq!(check_interval(&ten_years.issue_new("n1").unwrap().0), Duration::from_secs(3600), "at most hourly");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
