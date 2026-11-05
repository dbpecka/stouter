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

use crate::config::{ConfigSignature, DynamicConfig, NodeRecord, SignedBeat, SignedNode};
use crate::state::SharedState;
use crate::tls::TlsContext;

/// Prefixes of signed messages, one per kind, so a signature on one kind of
/// state can't be passed off as a signature on another.
const CONFIG_DOMAIN: &[u8] = b"stouter signed config v1\n";
const NODE_DOMAIN: &[u8] = b"stouter node record v1\n";
const BEAT_DOMAIN: &[u8] = b"stouter node beat v1\n";

/// A signing key and the certificate vouching for it.
pub struct Signer {
    cert: CertificateDer<'static>,
    key: Arc<dyn SigningKey>,
}

impl Signer {
    pub fn new(cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<Self> {
        let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(key)
            .map_err(|e| anyhow!("load signing key: {e}"))?;
        Ok(Self { cert, key })
    }

    /// The signer for a node's (or the CLI's) own TLS identity.
    pub fn from_material(material: &crate::pki::PkiMaterial) -> Result<Self> {
        Self::new(material.node_cert_der.clone(), &material.node_key)
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
}

/// Check that `signature` (hex) over `domain` + `text` was made by the key
/// of `cert` (hex DER), and that `cert` is valid for this cluster. Returns
/// the signer's Common Name.
fn verify_text(domain: &[u8], text: &str, cert: &str, signature: &str, tls: &TlsContext) -> Result<String> {
    let cert = CertificateDer::from(unhex(cert).context("signer certificate")?);
    let signer = tls
        .verify_cluster_cert(&cert)?
        .ok_or_else(|| anyhow!("the signer's certificate has no CN"))?;
    let end_entity = webpki::EndEntityCert::try_from(&cert).map_err(|e| anyhow!("signer certificate: {e:?}"))?;
    let signature = unhex(signature).context("signature")?;
    end_entity
        .verify_signature(webpki::aws_lc_rs::ECDSA_P256_SHA256, &[domain, text.as_bytes()].concat(), &signature)
        .map_err(|_| anyhow!("the signature does not verify"))?;
    Ok(signer)
}

// ---------------------------------------------------------------------------
// Configs
// ---------------------------------------------------------------------------

/// `dc`, signed by `signer`.
pub fn sign(dc: &DynamicConfig, signer: &Signer) -> Result<DynamicConfig> {
    let mut signed = dc.clone();
    signed.signature = None;
    let config_json = serde_json::to_string(&signed)?;
    let signature = signer.sign(CONFIG_DOMAIN, &config_json)?;
    signed.signature = Some(ConfigSignature { config_json, signer_cert: hex(signer.cert.as_ref()), signature });
    Ok(signed)
}

/// The config `dc`'s signature vouches for, and the admin who signed it.
///
/// Fails unless the signer's certificate chains to the cluster CA, its CN is
/// in `admin_cns`, and the signature verifies. The config returned is decoded
/// from the signed text (with the signature attached), so whatever happened
/// to the rest of `dc` on its way here doesn't matter.
pub fn verify(dc: &DynamicConfig, tls: &TlsContext, admin_cns: &[String]) -> Result<(DynamicConfig, String)> {
    let sig = dc
        .signature
        .as_ref()
        .ok_or_else(|| anyhow!("config version {} is not signed", dc.version))?;
    let signer = verify_text(CONFIG_DOMAIN, &sig.config_json, &sig.signer_cert, &sig.signature, tls)
        .map_err(|e| anyhow!("config version {}: {e:#}", dc.version))?;
    if !admin_cns.contains(&signer) {
        bail!("config version {} is signed by '{signer}', which is not in adminCns", dc.version);
    }
    let mut config: DynamicConfig = serde_json::from_str(&sig.config_json).context("decode signed config")?;
    config.signature = Some(sig.clone());
    Ok((config, signer))
}

/// The config this node may adopt for `incoming`: as is, or, with
/// `requireSignedConfig`, only if an admin signed it (decoded from the
/// signed text).
pub fn admit(state: &SharedState, incoming: DynamicConfig) -> Result<DynamicConfig> {
    if !state.config.require_signed_config {
        return Ok(incoming);
    }
    let tls = state.tls.as_deref().ok_or_else(|| anyhow!("no TLS context to check signatures with"))?;
    Ok(verify(&incoming, tls, &state.config.admin_cns)?.0)
}

/// Report a config refused by [`admit`]: a warning the first time a given
/// version is refused, debug after that (peers keep offering it every sync).
pub fn log_refusal(state: &SharedState, version: u64, err: &anyhow::Error) {
    if state.rejected_config_version.swap(version, Ordering::Relaxed) != version {
        warn!("refusing config version {version}: {err:#} (requireSignedConfig)");
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
    let signature = signer.sign(NODE_DOMAIN, &record_json)?;
    Ok(SignedNode { record_json, cert: hex(signer.cert.as_ref()), signature })
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
    let signature = signer.sign(BEAT_DOMAIN, &beat_text(id, beat))?;
    Ok(SignedBeat { id: id.to_string(), beat, signature })
}

/// Check that `beat` was signed with the key of `cert` (hex DER): the
/// certificate of the node's record, already verified when the record was.
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
        assert_eq!(DynamicConfig { signature: None, ..verified.clone() }, sample());
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
}
