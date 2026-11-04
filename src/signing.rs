//! Admin-signed configs (`requireSignedConfig`).
//!
//! Gossip carries the cluster's config through every node, and any node with
//! a cluster certificate can send one. With `requireSignedConfig`, a node
//! accepts a config change only if an admin signed it: a certificate from the
//! cluster CA whose CN is in that node's `adminCns`. The signature covers the
//! exact JSON text of the config, which travels with it, so any node can
//! relay a signed config but none can alter it.

use std::sync::atomic::Ordering;

use anyhow::{anyhow, bail, Context, Result};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{self, SignatureScheme};
use tracing::{debug, warn};

use crate::config::{ConfigSignature, DynamicConfig};
use crate::state::SharedState;
use crate::tls::TlsContext;

/// Prefix of every signed message, so a config signature can't be passed off
/// as a signature over anything else.
const DOMAIN: &[u8] = b"stouter signed config v1\n";

fn message(config_json: &str) -> Vec<u8> {
    [DOMAIN, config_json.as_bytes()].concat()
}

/// `dc`, signed by the holder of `key`, whose certificate is `cert`.
pub fn sign(dc: &DynamicConfig, cert: &CertificateDer<'_>, key: &PrivateKeyDer<'_>) -> Result<DynamicConfig> {
    let mut signed = dc.clone();
    signed.signature = None;
    let config_json = serde_json::to_string(&signed)?;
    let key = rustls::crypto::aws_lc_rs::sign::any_supported_type(key)
        .map_err(|e| anyhow!("load signing key: {e}"))?;
    let signer = key
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .ok_or_else(|| anyhow!("the signing key is not an ECDSA P-256 key"))?;
    let signature = signer.sign(&message(&config_json)).map_err(|e| anyhow!("sign config: {e}"))?;
    signed.signature = Some(ConfigSignature {
        config_json,
        signer_cert: hex(cert.as_ref()),
        signature: hex(&signature),
    });
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
    let cert = CertificateDer::from(unhex(&sig.signer_cert).context("signer certificate")?);
    let signer = tls
        .verify_cluster_cert(&cert)?
        .ok_or_else(|| anyhow!("the signer's certificate has no CN"))?;
    if !admin_cns.contains(&signer) {
        bail!("config version {} is signed by '{signer}', which is not in adminCns", dc.version);
    }
    let end_entity = webpki::EndEntityCert::try_from(&cert).map_err(|e| anyhow!("signer certificate: {e:?}"))?;
    let signature = unhex(&sig.signature).context("signature")?;
    end_entity
        .verify_signature(webpki::aws_lc_rs::ECDSA_P256_SHA256, &message(&sig.config_json), &signature)
        .map_err(|_| anyhow!("the signature on config version {} does not verify", dc.version))?;
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
        let signed = sign(&sample(), &admin.node_cert_der, &admin.node_key).unwrap();
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
        let signed = sign(&sample(), &admin.node_cert_der, &admin.node_key).unwrap();

        let mut forged = signed.clone();
        let sig = forged.signature.as_mut().unwrap();
        sig.config_json = sig.config_json.replace("3000", "4000");
        assert!(verify(&forged, &node, &admins).unwrap_err().to_string().contains("does not verify"));

        assert!(verify(&sample(), &node, &admins).unwrap_err().to_string().contains("not signed"));

        // Signed by a valid cluster certificate that isn't an admin's.
        let (mallory, _) = setup(&dir, "mallory");
        let by_mallory = sign(&sample(), &mallory.node_cert_der, &mallory.node_key).unwrap();
        assert!(verify(&by_mallory, &node, &admins).unwrap_err().to_string().contains("not in adminCns"));

        // An "admin" certificate from some other CA.
        let other = temp_dir("other");
        let (foreign_admin, _) = setup(&other, "admin");
        let foreign = sign(&sample(), &foreign_admin.node_cert_der, &foreign_admin.node_key).unwrap();
        assert!(verify(&foreign, &node, &admins).unwrap_err().to_string().contains("not valid for this cluster"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }
}
