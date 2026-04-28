use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::info;

/// The fixed DNS SAN stamped into every node cert so WebPki hostname
/// verification can be used without per-node cert customisation.
pub const MESH_SERVER_NAME: &str = "stouter-node";

pub struct PkiMaterial {
    pub ca_cert_der: CertificateDer<'static>,
    pub node_cert_der: CertificateDer<'static>,
    pub node_key: PrivateKeyDer<'static>,
}

/// Load PKI from `tls_dir` if the CA cert + key are present, generating them
/// (and a fresh node cert) otherwise.  The node cert is always regenerated at
/// startup so it rotates without operator intervention.
pub fn load_or_create(tls_dir: &str, node_id: &str) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create TLS directory {tls_dir}"))?;

    let ca_cert_path = dir.join("ca.crt");
    let ca_key_path = dir.join("ca.key");

    let (ca_cert_der, ca_cert_rcgen, ca_key) =
        if ca_cert_path.exists() && ca_key_path.exists() {
            load_ca(&ca_cert_path, &ca_key_path)?
        } else {
            generate_ca(&ca_cert_path, &ca_key_path)?
        };

    let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key)?;

    Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
}

// ---------------------------------------------------------------------------
// CA helpers
// ---------------------------------------------------------------------------

fn generate_ca(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(CertificateDer<'static>, rcgen::Certificate, KeyPair)> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate CA key")?;

    let mut params = CertificateParams::new(Vec::<String>::new())
        .context("CA cert params")?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, "stouter-cluster-ca");
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2044, 1, 1);

    let cert = params.self_signed(&key).context("self-sign CA")?;
    let der = CertificateDer::from(cert.der().to_vec());

    std::fs::write(cert_path, cert.pem())
        .with_context(|| format!("write {}", cert_path.display()))?;
    std::fs::write(key_path, key.serialize_pem())
        .with_context(|| format!("write {}", key_path.display()))?;

    info!("generated CA certificate → {}", cert_path.display());
    Ok((der, cert, key))
}

fn load_ca(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(CertificateDer<'static>, rcgen::Certificate, KeyPair)> {
    let cert_pem = std::fs::read_to_string(cert_path)
        .with_context(|| format!("read {}", cert_path.display()))?;
    let key_pem = std::fs::read_to_string(key_path)
        .with_context(|| format!("read {}", key_path.display()))?;

    let der = pem_to_der(&cert_pem).context("decode CA cert")?;
    let key = KeyPair::from_pem(&key_pem).context("parse CA key")?;
    let params = CertificateParams::from_ca_cert_der(&der).context("parse CA cert params")?;
    let cert = params.self_signed(&key).context("re-create CA cert")?;

    info!("loaded CA certificate ← {}", cert_path.display());
    Ok((der, cert, key))
}

// ---------------------------------------------------------------------------
// Node cert
// ---------------------------------------------------------------------------

fn generate_node_cert(
    node_id: &str,
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate node key")?;

    let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()])
        .context("node cert params")?;
    params.distinguished_name.push(DnType::CommonName, node_id);
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2034, 1, 1);

    let cert = params.signed_by(&key, ca_cert, ca_key)
        .context("sign node cert with CA")?;

    let der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

    Ok((der, key_der))
}

// ---------------------------------------------------------------------------
// PEM decode helpers
// ---------------------------------------------------------------------------

fn pem_to_der(pem: &str) -> Result<CertificateDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let cert = rustls_pemfile::certs(&mut reader)
        .next()
        .context("no certificate in PEM")?
        .context("parse PEM certificate")?;
    Ok(cert)
}
