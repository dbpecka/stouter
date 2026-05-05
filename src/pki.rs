use std::path::Path;

use anyhow::{bail, Context, Result};
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

/// Load PKI material from `tls_dir` using the following precedence:
///
/// 1. `ca.crt` + `ca.key` present → load the CA and generate a fresh node cert
///    in memory (rotates on every restart).
/// 2. `ca.crt` + `node.crt` + `node.key` present (no `ca.key`) → load the
///    pre-issued cert written by `cert issue` on another node.
/// 3. Nothing present → generate a new CA (written to disk) and a fresh node
///    cert (in memory only).
pub fn load_or_create(tls_dir: &str, node_id: &str) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create TLS directory {tls_dir}"))?;

    let ca_cert_path  = dir.join("ca.crt");
    let ca_key_path   = dir.join("ca.key");
    let node_cert_path = dir.join("node.crt");
    let node_key_path  = dir.join("node.key");

    if ca_cert_path.exists() && ca_key_path.exists() {
        let (ca_cert_der, ca_cert_rcgen, ca_key) = load_ca(&ca_cert_path, &ca_key_path)?;
        let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key)?;
        Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
    } else if ca_cert_path.exists() && node_cert_path.exists() && node_key_path.exists() {
        load_issued(&ca_cert_path, &node_cert_path, &node_key_path)
    } else {
        let (ca_cert_der, ca_cert_rcgen, ca_key) = generate_ca(&ca_cert_path, &ca_key_path)?;
        let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key)?;
        Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
    }
}

/// Issue a leaf certificate for `node_id`, signed by the CA in `tls_dir`, and
/// write `ca.crt`, `node.crt`, and `node.key` (all PEM) to `out_dir`.
///
/// The three files can be copied directly into the target node's `tlsDir`.
pub fn issue_cert_to_dir(tls_dir: &str, node_id: &str, out_dir: &str) -> Result<()> {
    let dir = Path::new(tls_dir);
    let ca_cert_path = dir.join("ca.crt");
    let ca_key_path  = dir.join("ca.key");

    if !ca_cert_path.exists() || !ca_key_path.exists() {
        bail!("CA not found in {tls_dir}; start the daemon once to generate it");
    }

    let (_, ca_cert_rcgen, ca_key) = load_ca(&ca_cert_path, &ca_key_path)?;

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate node key")?;

    let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()])
        .context("node cert params")?;
    params.distinguished_name.push(DnType::CommonName, node_id);
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after  = rcgen::date_time_ymd(2034, 1, 1);

    let cert = params.signed_by(&key, &ca_cert_rcgen, &ca_key)
        .context("sign node cert with CA")?;

    let out = Path::new(out_dir);
    std::fs::create_dir_all(out)
        .with_context(|| format!("create output directory {out_dir}"))?;

    let ca_pem = std::fs::read_to_string(&ca_cert_path)
        .with_context(|| format!("read {}", ca_cert_path.display()))?;
    std::fs::write(out.join("ca.crt"), &ca_pem).context("write ca.crt")?;
    std::fs::write(out.join("node.crt"), cert.pem()).context("write node.crt")?;
    std::fs::write(out.join("node.key"), key.serialize_pem()).context("write node.key")?;

    info!("issued certificate for '{node_id}' → {out_dir}");
    Ok(())
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
// Pre-issued cert loader
// ---------------------------------------------------------------------------

fn load_issued(
    ca_cert_path: &Path,
    node_cert_path: &Path,
    node_key_path: &Path,
) -> Result<PkiMaterial> {
    let ca_pem = std::fs::read_to_string(ca_cert_path)
        .with_context(|| format!("read {}", ca_cert_path.display()))?;
    let node_cert_pem = std::fs::read_to_string(node_cert_path)
        .with_context(|| format!("read {}", node_cert_path.display()))?;
    let node_key_pem = std::fs::read_to_string(node_key_path)
        .with_context(|| format!("read {}", node_key_path.display()))?;

    let ca_cert_der   = pem_to_der(&ca_pem).context("decode CA cert")?;
    let node_cert_der = pem_to_der(&node_cert_pem).context("decode node cert")?;
    let node_key      = pem_to_private_key(&node_key_pem).context("decode node key")?;

    info!("loaded pre-issued certificate ← {}", node_cert_path.display());
    Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
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

fn pem_to_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)
        .context("parse private key PEM")?
        .context("no private key in PEM")
}
