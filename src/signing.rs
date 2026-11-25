//! Signatures on gossiped state.
//!
//! Gossip carries state through every node, and any node with a cluster
//! certificate can send some. Two kinds of it are signed, so that any node
//! can relay them but none can alter them:
//!
//! - **Node records**: what each node says about itself (address,
//!   heartbeat, health, the relays it uses), signed with its own
//!   certificate's key. Peers believe what they hear about node X only if X
//!   signed it, so no node can redirect, hide or fake another.
//! - **Configs** (`requireSignedConfig`): a config change counts only if an
//!   admin signed it, a certificate from the cluster CA whose CN is in the
//!   receiving node's `adminCns`.
//!
//! Each signature covers the exact JSON text of what was signed, which
//! travels with it, so it verifies however the rest was re-encoded on the
//! way (an older node may drop fields it doesn't know). What a receiver uses
//! is always decoded from that text.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::sign::SigningKey;
use tokio_rustls::rustls::{self, SignatureScheme};
use tracing::{debug, warn};

use crate::config::{ConfigSignature, DynamicConfig, NodeRecord, SignedBeat, SignedCaller, SignedNode};
use crate::state::SharedState;
use crate::tls::TlsContext;

/// Prefixes of signed messages, one per kind, so a signature on one kind of
/// state can't be passed off as a signature on another.
const CONFIG_DOMAIN: &[u8] = b"stouter signed config v1\n";
const NODE_DOMAIN: &[u8] = b"stouter node record v1\n";
const BEAT_DOMAIN: &[u8] = b"stouter node beat v1\n";
const TUNNEL_DOMAIN: &[u8] = b"stouter tunnel request v1\n";

/// A signing key and the certificate vouching for it, replaced together
/// when the certificate is renewed (see `crate::renew`).
pub struct Signer {
    current: arc_swap::ArcSwap<SignerKey>,
}

/// One certificate and its key: a signature and the certificate sent with
/// it always come from the same one.
struct SignerKey {
    cert: CertificateDer<'static>,
    key: Arc<dyn SigningKey>,
}

impl SignerKey {
    fn new(cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<Self> {
        let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(key)
            .map_err(|e| anyhow!("load signing key: {e}"))?;
        Ok(Self { cert, key })
    }

    /// Hex signature over `domain` + `text`.
    fn sign(&self, domain: &[u8], text: &str) -> Result<String> {
        let signer = self
            .key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
            .ok_or_else(|| anyhow!("the signing key is not an ECDSA P-256 key"))?;
        let signature = signer
            .sign(&[domain, text.as_bytes()].concat())
            .map_err(|e| anyhow!("sign: {e}"))?;
        Ok(hex(&signature))
    }

    fn cert_hex(&self) -> String {
        hex(self.cert.as_ref())
    }
}

impl Signer {
    pub fn new(cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<Self> {
        Ok(Self { current: arc_swap::ArcSwap::from_pointee(SignerKey::new(cert, key)?) })
    }

    /// The signer for a node's (or the CLI's) own TLS identity.
    pub fn from_material(material: &crate::pki::PkiMaterial) -> Result<Self> {
        Self::new(material.node_cert_der.clone(), &material.node_key)
    }

    /// Sign with `cert` and `key` from now on (a renewed certificate).
    pub fn replace(&self, cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<()> {
        self.current.store(Arc::new(SignerKey::new(cert, key)?));
        Ok(())
    }

    fn current(&self) -> Arc<SignerKey> {
        self.current.load_full()
    }
}

/// Check that `signature` (hex) over `domain` + `text` was made by the key
/// of `cert` (hex DER), and that `cert` is valid for this cluster. Returns
/// the signer's Common Name.
fn verify_text(domain: &[u8], text: &str, cert: &str, signature: &str, tls: &TlsContext) -> Result<String> {
    verify_text_at(domain, text, cert, signature, tls, rustls::pki_types::UnixTime::now())
}

/// [`verify_text`], with `cert` checked as of `at` (when it signed).
fn verify_text_at(
    domain: &[u8],
    text: &str,
    cert: &str,
    signature: &str,
    tls: &TlsContext,
    at: rustls::pki_types::UnixTime,
) -> Result<String> {
    let cert = CertificateDer::from(unhex(cert).context("signer certificate")?);
    let signer = tls
        .verify_cluster_cert_at(&cert, at)?
        .ok_or_else(|| anyhow!("the signer's certificate has no CN"))?;
    let end_entity = webpki::EndEntityCert::try_from(&cert).map_err(|e| anyhow!("signer certificate: {e:?}"))?;
    let signature = unhex(signature).context("signature")?;
    end_entity
        .verify_signature(webpki::aws_lc_rs::ECDSA_P256_SHA256, &[domain, text.as_bytes()].concat(), &signature)
        .map_err(|_| anyhow!("the signature does not verify"))?;
    Ok(signer)
}

// ---------------------------------------------------------------------------
// Tunnel requests
// ---------------------------------------------------------------------------

/// The text a caller signs for a tunnel request.
fn tunnel_text(node_id: &str, service: &str, timestamp_ms: u64, nonce: u64) -> String {
    serde_json::to_string(&(node_id, service, timestamp_ms, nonce)).expect("plain values encode")
}

/// `signer`'s signature over a tunnel request to `service` on `node_id`.
pub fn sign_tunnel_request(signer: &Signer, node_id: &str, service: &str, timestamp_ms: u64) -> Result<SignedCaller> {
    let nonce = rand::random::<u64>();
    let key = signer.current();
    let signature = key.sign(TUNNEL_DOMAIN, &tunnel_text(node_id, service, timestamp_ms, nonce))?;
    Ok(SignedCaller { nonce, cert: key.cert_hex(), signature })
}

/// Who signed `caller` for a tunnel request to `service` on `node_id` at
/// `timestamp_ms`: the Common Name of a certificate from the cluster CA
/// (not revoked) whose key made the signature.
pub fn verify_tunnel_request(
    caller: &SignedCaller,
    node_id: &str,
    service: &str,
    timestamp_ms: u64,
    tls: &TlsContext,
) -> Result<String> {
    let text = tunnel_text(node_id, service, timestamp_ms, caller.nonce);
    verify_text(TUNNEL_DOMAIN, &text, &caller.cert, &caller.signature, tls)
}

// ---------------------------------------------------------------------------
// Configs
// ---------------------------------------------------------------------------

/// How far apart the clocks of the hosts signing configs may be: a config
/// may claim to be signed up to this far in the future, or this far before
/// the config a node holds.
pub const SIGNING_CLOCK_SLACK: u64 = 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `dc`, signed by `signer` now.
pub fn sign(dc: &DynamicConfig, signer: &Signer) -> Result<DynamicConfig> {
    sign_at(dc, signer, now_secs())
}

/// `dc`, signed by `signer`, saying it was signed at `at` (Unix seconds).
pub(crate) fn sign_at(dc: &DynamicConfig, signer: &Signer, at: u64) -> Result<DynamicConfig> {
    let mut signed = dc.clone();
    signed.signature = None;
    signed.signed_at = Some(at);
    let config_json = serde_json::to_string(&signed)?;
    let key = signer.current();
    let signature = key.sign(CONFIG_DOMAIN, &config_json)?;
    signed.signature = Some(ConfigSignature { config_json, signer_cert: key.cert_hex(), signature });
    Ok(signed)
}

/// The config `dc`'s signature vouches for, and the admin who signed it.
///
/// Fails unless the signer's certificate chains to the cluster CA, its CN is
/// in `admin_cns`, and the signature verifies. The config returned is decoded
/// from the signed text (with the signature attached), so whatever happened
/// to the rest of `dc` on its way here doesn't matter.
///
/// The certificate is checked as of when the config says it was signed
/// (`signedAt`, part of what was signed), so a config outlives its signer's
/// certificate; whether it is revoked, as of now. A config dated more than
/// [`SIGNING_CLOCK_SLACK`] ahead is refused. One with no date (signed by an
/// older version) is checked as of now.
pub fn verify(dc: &DynamicConfig, tls: &TlsContext, admin_cns: &[String]) -> Result<(DynamicConfig, String)> {
    let sig = dc
        .signature
        .as_ref()
        .ok_or_else(|| anyhow!("config version {} is not signed", dc.version))?;
    let mut config: DynamicConfig = serde_json::from_str(&sig.config_json).context("decode signed config")?;
    let at = match config.signed_at {
        Some(at) if at > now_secs() + SIGNING_CLOCK_SLACK => bail!(
            "config version {} says it was signed at {}, in the future (is the signing host's clock right?)",
            dc.version,
            crate::history::format_secs(at)
        ),
        Some(at) => rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(at)),
        None => rustls::pki_types::UnixTime::now(),
    };
    let signer = verify_text_at(CONFIG_DOMAIN, &sig.config_json, &sig.signer_cert, &sig.signature, tls, at)
        .map_err(|e| anyhow!("config version {}: {e:#}", dc.version))?;
    if !admin_cns.contains(&signer) {
        bail!("config version {} is signed by '{signer}', which is not in adminCns", dc.version);
    }
    // A config revoking the certificate that signed it would leave nothing
    // vouching for it: refused, rather than adopted as signed by no one.
    let signer_fp = crate::tls::fingerprint_hex(&unhex(&sig.signer_cert).context("signer certificate")?);
    if config.revoked.nodes.contains(&signer) || config.revoked.certs.contains(&signer_fp) {
        bail!("config version {} revokes the certificate that signed it ('{signer}'): another admin must sign it", dc.version);
    }
    config.signature = Some(sig.clone());
    Ok((config, signer))
}

/// The config this node may adopt for `incoming`, holding `held`: as is,
/// or, with `requireSignedConfig`, only if an admin signed it (decoded from
/// the signed text), and not well before `held` was (see [`signed_too_early`]).
/// Either way, not one whose version is implausibly far past `held`'s (see
/// [`crate::config::too_far_ahead`]).
pub fn admit(state: &SharedState, incoming: DynamicConfig, held: &DynamicConfig) -> Result<DynamicConfig> {
    if !state.config.require_signed_config {
        if let Some(why) = crate::config::too_far_ahead(&incoming, held) {
            bail!("{why}");
        }
        return Ok(incoming);
    }
    let tls = state.tls.as_deref().ok_or_else(|| anyhow!("no TLS context to check signatures with"))?;
    let config = verify(&incoming, tls, &state.config.admin_cns)?.0;
    admit_verified(state, config, held)
}

/// The rest of [`admit`] (with `requireSignedConfig`), for a `config`
/// [`verify`] has already decoded and checked: the cheap part, against the
/// config `held` now. So a node can verify a signature (the costly part)
/// without holding its config's lock, then decide under it.
pub fn admit_verified(state: &SharedState, config: DynamicConfig, held: &DynamicConfig) -> Result<DynamicConfig> {
    if let Some(why) = signed_too_early(&config, held).or_else(|| crate::config::too_far_ahead(&config, held)) {
        bail!("{why}");
    }
    // Checked as of when it says it was signed, the signer's certificate
    // was valid; if it has expired since, its key may have signed this
    // only now, dated back. A node catching up after it starts takes it
    // (it is likely the cluster's config, signed while that certificate
    // was valid); a running node is past that, and takes only configs
    // whose signer is still valid.
    if let Some((who, until)) = expired_signer(&config) {
        if !state.catching_up() {
            bail!("{}", expired_signer_refusal(&config, &who, until));
        }
        warn!(
            "taking config version {} signed by '{who}', whose certificate expired {}, as this node is catching up \
             after starting; have an admin with a current certificate sign the cluster's config again",
            config.version,
            crate::history::format_secs(until)
        );
    }
    Ok(config)
}

/// Who signed `dc` (their certificate's CN) and when that certificate
/// expired (Unix seconds), if it has expired by now. Taken from the
/// signature as it stands: meant for a config already verified.
pub fn expired_signer(dc: &DynamicConfig) -> Option<(String, u64)> {
    let sig = dc.signature.as_ref()?;
    let der = unhex(&sig.signer_cert).ok()?;
    let (_, until) = crate::pki::validity(&der)?;
    let until = u64::try_from(until).unwrap_or(0);
    (until < now_secs()).then(|| (crate::tls::cert_cn(&der.into()).unwrap_or_else(|| "?".into()), until))
}

/// Why a running node won't take `config`, signed by `who` with a
/// certificate that expired at `until`.
pub fn expired_signer_refusal(config: &DynamicConfig, who: &str, until: u64) -> String {
    format!(
        "config version {} was signed by '{who}', whose certificate expired {}: once it has caught up after \
         starting, a node takes no config signed with an expired certificate, as that key could still sign \
         configs dated back; have an admin with a current certificate sign the cluster's config again \
         (`stouter config push`)",
        config.version,
        crate::history::format_secs(until)
    )
}

/// Why `incoming` mustn't replace `held`, if its signer's certificate has
/// expired and it says it was signed more than [`SIGNING_CLOCK_SLACK`]
/// before `held` was. A certificate is checked as of when it signed, so an
/// expired key (from an old backup, say) can still sign configs dated
/// within its life: this is what keeps those from replacing anything
/// signed after it expired. A key still valid gains nothing by dating a
/// config back (it can sign one dated now), so its configs aren't
/// compared: one made on the other side of a partition, earlier but at a
/// higher version, must still be taken. Nor are configs with no date
/// (signed by older versions).
pub fn signed_too_early(incoming: &DynamicConfig, held: &DynamicConfig) -> Option<String> {
    let (Some(new), Some(old)) = (incoming.signed_at, held.signed_at) else { return None };
    let (who, until) = expired_signer(incoming)?;
    (new + SIGNING_CLOCK_SLACK < old).then(|| {
        format!(
            "config version {} was signed at {} by '{who}', whose certificate expired {}, well before the config \
             held here (version {}, signed at {})",
            incoming.version,
            crate::history::format_secs(new),
            crate::history::format_secs(until),
            held.version,
            crate::history::format_secs(old)
        )
    })
}

/// Report a config refused by [`admit`]: a warning the first time a given
/// version is refused, debug after that (peers keep offering it every sync).
pub fn log_refusal(state: &SharedState, version: u64, err: &anyhow::Error) {
    if state.rejected_config_version.swap(version, Ordering::Relaxed) != version {
        warn!("refusing config version {version}: {err:#}");
    } else {
        debug!("refusing config version {version} again: {err:#}");
    }
}

// ---------------------------------------------------------------------------
// Node records
// ---------------------------------------------------------------------------

/// `record`, signed by the node it describes.
pub fn sign_node(record: &NodeRecord, signer: &Signer) -> Result<SignedNode> {
    let record_json = serde_json::to_string(record)?;
    let key = signer.current();
    let signature = key.sign(NODE_DOMAIN, &record_json)?;
    Ok(SignedNode { record_json, cert: key.cert_hex(), signature })
}

/// The heartbeat a signed record claims, read without verifying it: enough
/// to skip a record that's no newer than one already held (the common case,
/// as every peer relays every record each round) before paying for a check.
pub fn claimed_heartbeat(signed: &SignedNode) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Heartbeat {
        #[serde(default)]
        heartbeat: u64,
    }
    serde_json::from_str::<Heartbeat>(&signed.record_json).ok().map(|h| h.heartbeat)
}

/// The record `signed` vouches for. Fails unless the certificate is valid
/// for this cluster, names the very node the record describes, and the
/// signature verifies: only node X can say anything about X.
pub fn verify_node(signed: &SignedNode, tls: &TlsContext) -> Result<NodeRecord> {
    let signer = verify_text(NODE_DOMAIN, &signed.record_json, &signed.cert, &signed.signature, tls)?;
    let record: NodeRecord = serde_json::from_str(&signed.record_json).context("decode node record")?;
    if record.id != signer {
        bail!("a record for node '{}' signed by '{signer}'", record.id);
    }
    Ok(record)
}

// ---------------------------------------------------------------------------
// Beats
// ---------------------------------------------------------------------------

fn beat_text(id: &str, beat: u64) -> String {
    format!("{id}\n{beat}")
}

/// A beat for node `id`, signed by it.
pub fn sign_beat(id: &str, beat: u64, signer: &Signer) -> Result<SignedBeat> {
    let signature = signer.current().sign(BEAT_DOMAIN, &beat_text(id, beat))?;
    Ok(SignedBeat { id: id.to_string(), beat, signature })
}

/// Check that `beat` was signed with the key of `cert` (hex DER): the
/// certificate of the node's record, already verified when the record was.
/// Whether the certificate `cert` (DER, hex) has expired, or can't be read:
/// a beat signed with its key says nothing about the node now. Records are
/// checked against the cluster CA as of now, so this matters for beats,
/// which are checked against the certificate of a record already held.
pub fn cert_hex_expired(cert: &str) -> bool {
    unhex(cert).ok().and_then(|der| crate::pki::validity(&der)).is_none_or(|(_, until)| {
        u64::try_from(until).unwrap_or(0) < now_secs()
    })
}

pub fn verify_beat(beat: &SignedBeat, cert: &str) -> Result<()> {
    let cert = CertificateDer::from(unhex(cert).context("certificate")?);
    let end_entity = webpki::EndEntityCert::try_from(&cert).map_err(|e| anyhow!("certificate: {e:?}"))?;
    let signature = unhex(&beat.signature).context("signature")?;
    end_entity
        .verify_signature(
            webpki::aws_lc_rs::ECDSA_P256_SHA256,
            &[BEAT_DOMAIN, beat_text(&beat.id, beat.beat).as_bytes()].concat(),
            &signature,
        )
        .map_err(|_| anyhow!("the beat's signature does not verify"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        bail!("not hex");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| anyhow!("not hex")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Service, ServiceGroup};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stouter_sign_{tag}_{}_{}", std::process::id(), rand::random::<u64>()))
    }

    fn sample() -> DynamicConfig {
        let svc: Service = serde_json::from_value(serde_json::json!({"name": "api", "nodeId": "n1", "nodePort": 3000})).unwrap();
        DynamicConfig {
            version: 7,
            service_groups: vec![ServiceGroup { name: "g".into(), services: vec![svc] }],
            changed_by: "admin".into(),
            ..Default::default()
        }
    }

    /// (material for `signer`, a node's TLS context), sharing the CA in `dir`.
    fn setup(dir: &std::path::Path, signer: &str) -> (crate::pki::PkiMaterial, TlsContext) {
        let material = crate::pki::load_or_create(dir.to_str().unwrap(), signer).unwrap();
        let node = TlsContext::new(&crate::pki::load_or_create(dir.to_str().unwrap(), "node").unwrap()).unwrap();
        (material, node)
    }

    #[test]
    fn a_signed_config_verifies_and_comes_back_as_signed() {
        let dir = temp_dir("ok");
        let (admin, node) = setup(&dir, "admin");
        let signed = sign(&sample(), &Signer::from_material(&admin).unwrap()).unwrap();
        let (verified, signer) = verify(&signed, &node, &["admin".into()]).unwrap();
        assert_eq!(signer, "admin");
        assert_eq!(DynamicConfig { signature: None, signed_at: None, ..verified.clone() }, sample());
        assert!(verified.signed_at.is_some_and(|at| at.abs_diff(now_secs()) < 60), "dated when signed");
        assert_eq!(verified, signed);

        // A relay that altered the envelope changes nothing: the signed
        // text is what counts.
        let mut tampered_envelope = signed.clone();
        tampered_envelope.version = 99;
        tampered_envelope.service_groups.clear();
        assert_eq!(verify(&tampered_envelope, &node, &["admin".into()]).unwrap().0, signed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_unsigned_or_foreign_configs_are_refused() {
        let dir = temp_dir("bad");
        let (admin, node) = setup(&dir, "admin");
        let admins = ["admin".to_string()];
        let signed = sign(&sample(), &Signer::from_material(&admin).unwrap()).unwrap();

        let mut forged = signed.clone();
        let sig = forged.signature.as_mut().unwrap();
        sig.config_json = sig.config_json.replace("3000", "4000");
        assert!(verify(&forged, &node, &admins).unwrap_err().to_string().contains("does not verify"));

        assert!(verify(&sample(), &node, &admins).unwrap_err().to_string().contains("not signed"));

        // Signed by a valid cluster certificate that isn't an admin's.
        let (mallory, _) = setup(&dir, "mallory");
        let by_mallory = sign(&sample(), &Signer::from_material(&mallory).unwrap()).unwrap();
        assert!(verify(&by_mallory, &node, &admins).unwrap_err().to_string().contains("not in adminCns"));

        // An "admin" certificate from some other CA.
        let other = temp_dir("other");
        let (foreign_admin, _) = setup(&other, "admin");
        let foreign = sign(&sample(), &Signer::from_material(&foreign_admin).unwrap()).unwrap();
        assert!(verify(&foreign, &node, &admins).unwrap_err().to_string().contains("not valid for this cluster"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }
    #[test]
    fn a_signed_config_outlives_its_signers_certificate_but_an_expired_key_signs_nothing_new() {
        let dir = temp_dir("expiry");
        let (_, node) = setup(&dir, "x");
        let admins = ["admin".to_string()];
        // An admin certificate that lasts two seconds.
        let issuer = crate::pki::Issuer::load(dir.to_str().unwrap(), time::Duration::seconds(2)).unwrap().unwrap();
        let (cert, key) = issuer.issue_new("admin").unwrap();
        let admin = Signer::new(cert, &key).unwrap();
        let issued = now_secs();
        let signed = sign(&sample(), &admin).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));

        // Expired since: what it signed while valid still verifies.
        assert!(verify(&signed, &node, &admins).is_ok());
        // Dated now, nothing it signs verifies.
        let late = sign(&DynamicConfig { version: 8, ..sample() }, &admin).unwrap();
        let err = verify(&late, &node, &admins).unwrap_err();
        assert!(format!("{err:#}").contains("not valid"), "{err:#}");
        // Dated back within its life it verifies, but it can't replace a
        // config signed after the key expired (by more than the clock slack).
        let backdated = sign_at(&DynamicConfig { version: 9, ..sample() }, &admin, issued).unwrap();
        assert!(verify(&backdated, &node, &admins).is_ok());
        let held = DynamicConfig { version: 8, signed_at: Some(issued + SIGNING_CLOCK_SLACK + 60), ..sample() };
        let why = signed_too_early(&backdated, &held).expect("refused");
        assert!(why.contains("well before the config held here"), "{why}");
        // Within the slack, or with either undated (an older version's), it isn't refused.
        assert!(signed_too_early(&backdated, &DynamicConfig { signed_at: Some(issued + 600), ..held.clone() }).is_none());
        assert!(signed_too_early(&backdated, &DynamicConfig { signed_at: None, ..held.clone() }).is_none());
        // Nor is a config signed hours before the held one by a key still
        // valid: made on the other side of a partition, at a higher version
        // (it could have been dated now; nothing is gained dating it back).
        let (current, _) = setup(&dir, "admin");
        let other_side = sign_at(&DynamicConfig { version: 9, ..sample() }, &Signer::from_material(&current).unwrap(), issued).unwrap();
        assert!(signed_too_early(&other_side, &held).is_none());

        // Dated in the future (beyond the slack): refused.
        let (fresh, _) = setup(&dir, "admin");
        let ahead = sign_at(&sample(), &Signer::from_material(&fresh).unwrap(), now_secs() + 2 * SIGNING_CLOCK_SLACK).unwrap();
        let err = verify(&ahead, &node, &admins).unwrap_err();
        assert!(format!("{err:#}").contains("in the future"), "{err:#}");

        // Revocation still counts as of now, whenever it signed.
        node.revocations().set(&crate::config::Revoked { nodes: vec!["admin".into()], certs: vec![] });
        assert!(verify(&signed, &node, &admins).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
