use std::path::Path;

use anyhow::{bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::{info, warn};

/// The fixed DNS SAN stamped into every node cert so WebPki hostname
/// verification can be used without per-node cert customisation.
pub const MESH_SERVER_NAME: &str = "stouter-node";

/// How long a node certificate is valid from the day it is issued (but
/// never past the CA's own expiry).
const NODE_CERT_VALIDITY: time::Duration = time::Duration::days(10 * 365);

/// How long a newly generated cluster CA is valid.
const CA_VALIDITY: time::Duration = time::Duration::days(20 * 365);

/// Certificates start this far in the past, so a peer whose clock runs a
/// little behind still accepts one issued just now.
const CLOCK_SKEW_ALLOWANCE: time::Duration = time::Duration::days(1);

/// Validity window for a node certificate signed by `ca`: from now (less
/// the skew allowance) for [`NODE_CERT_VALIDITY`], ending no later than the
/// CA does, since a certificate can't outlive the CA that vouches for it.
fn node_cert_validity(ca: &rcgen::Certificate) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (now - CLOCK_SKEW_ALLOWANCE, (now + NODE_CERT_VALIDITY).min(ca.params().not_after))
}

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
///
/// Anything else (some of the files, not a usable set) is an error, never a
/// new CA.
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
    } else if [&ca_cert_path, &ca_key_path, &node_cert_path, &node_key_path].iter().any(|p| p.exists()) {
        // Some of the files, not a usable set: most likely a copy that
        // missed one. Making a new CA here would overwrite the cluster's
        // `ca.crt` and cut this node off from it for good (its own new
        // `ca.key` then outranks the issued certificate).
        let have: Vec<&str> = ["ca.crt", "ca.key", "node.crt", "node.key"]
            .into_iter()
            .filter(|name| dir.join(name).exists())
            .collect();
        bail!(
            "{tls_dir} has {} but not a complete set: it needs ca.crt with ca.key (the CA's own directory), or \
             ca.crt with node.crt and node.key (from `stouter cert issue`). Copy the missing file(s), or empty the \
             directory to start a new cluster with a new CA",
            have.join(", ")
        )
    } else {
        let (ca_cert_der, ca_cert_rcgen, ca_key) = generate_ca(&ca_cert_path, &ca_key_path)?;
        let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key)?;
        Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
    }
}

/// Like [`load_or_create`], but never creates anything: for the CLI. A
/// directory with no certificates there is a mistyped `--cert-dir` (or a
/// node's `tlsDir` before its daemon first ran), not a new cluster, and a
/// CA made up on the spot would only fail the handshake later, obscurely.
pub fn load_existing(tls_dir: &str, node_id: &str) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);
    let has = |name: &str| dir.join(name).is_file();
    if has("ca.crt") && (has("ca.key") || (has("node.crt") && has("node.key"))) {
        return load_or_create(tls_dir, node_id);
    }
    if !dir.is_dir() {
        bail!("certificate directory {tls_dir} does not exist");
    }
    bail!(
        "no certificates in {tls_dir}: expected ca.crt, node.crt and node.key (as written by \
         `stouter cert issue`), or ca.crt and ca.key (the CA's own directory)"
    )
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
    (params.not_before, params.not_after) = node_cert_validity(&ca_cert_rcgen);

    let cert = params.signed_by(&key, &ca_cert_rcgen, &ca_key)
        .context("sign node cert with CA")?;

    let out = Path::new(out_dir);
    std::fs::create_dir_all(out)
        .with_context(|| format!("create output directory {out_dir}"))?;

    let ca_pem = std::fs::read_to_string(&ca_cert_path)
        .with_context(|| format!("read {}", ca_cert_path.display()))?;
    std::fs::write(out.join("ca.crt"), &ca_pem).context("write ca.crt")?;
    std::fs::write(out.join("node.crt"), cert.pem()).context("write node.crt")?;
    write_private_key(&out.join("node.key"), &key.serialize_pem()).context("write node.key")?;

    info!("issued certificate for '{node_id}' → {out_dir}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Private key files
// ---------------------------------------------------------------------------

/// Write a private key readable by its owner only (mode `0600` on Unix), so
/// other local users can't copy it, or, for `ca.key`, mint certificates
/// with any CN, `adminCns` included. An existing file is tightened before
/// the new key goes in.
fn write_private_key(path: &Path, pem: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // `mode` only applies when the file is created.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(pem.as_bytes())?;
    file.sync_all()
}

/// Warn when a private key file can be read by users other than its owner,
/// e.g. one written by an earlier version with the default `0644`.
fn warn_if_readable_by_others(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warn!(
                    "{} is readable by other users (mode {mode:03o}); restrict it with `chmod 600 {}`",
                    path.display(),
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - CLOCK_SKEW_ALLOWANCE;
    params.not_after = now + CA_VALIDITY;

    let cert = params.self_signed(&key).context("self-sign CA")?;
    let der = CertificateDer::from(cert.der().to_vec());

    std::fs::write(cert_path, cert.pem())
        .with_context(|| format!("write {}", cert_path.display()))?;
    write_private_key(key_path, &key.serialize_pem())
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
    warn_if_readable_by_others(key_path);
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
    (params.not_before, params.not_after) = node_cert_validity(ca_cert);

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
    warn_if_readable_by_others(node_key_path);
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_keys_are_owner_only() {
        let base = std::env::temp_dir().join(format!("stouter_pki_{}_{}", std::process::id(), rand::random::<u64>()));
        let tls = base.join("tls");
        let tls_s = tls.to_str().unwrap();
        load_or_create(tls_s, "n1").unwrap();
        assert_eq!(mode(&tls.join("ca.key")), 0o600);

        // Issuing over an existing, world-readable node.key tightens it.
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("node.key"), "old").unwrap();
        std::fs::set_permissions(out.join("node.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
        issue_cert_to_dir(tls_s, "n2", out.to_str().unwrap()).unwrap();
        assert_eq!(mode(&out.join("node.key")), 0o600);
        assert!(std::fs::read_to_string(out.join("node.key")).unwrap().contains("PRIVATE KEY"));

        // The issued material still loads.
        load_or_create(out.to_str().unwrap(), "n2").unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tls_dir_missing_a_file_is_refused_not_given_a_new_ca() {
        let base = std::env::temp_dir().join(format!("stouter_pki_{}_{}", std::process::id(), rand::random::<u64>()));
        let ca = base.join("ca");
        load_or_create(ca.to_str().unwrap(), "n1").unwrap();
        let issued = base.join("issued");
        issue_cert_to_dir(ca.to_str().unwrap(), "n2", issued.to_str().unwrap()).unwrap();
        // A copy that missed node.key.
        std::fs::remove_file(issued.join("node.key")).unwrap();
        let ca_before = std::fs::read(issued.join("ca.crt")).unwrap();
        let err = load_or_create(issued.to_str().unwrap(), "n2").err().expect("refused");
        assert!(err.to_string().contains("not a complete set"), "{err}");
        assert_eq!(std::fs::read(issued.join("ca.crt")).unwrap(), ca_before, "the cluster's CA is untouched");
        assert!(!issued.join("ca.key").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_cli_never_makes_up_a_ca() {
        let base = std::env::temp_dir().join(format!("stouter_pki_{}_{}", std::process::id(), rand::random::<u64>()));
        // A directory that doesn't exist, or holds no certificates: refused,
        // and nothing is created.
        let missing = base.join("typo");
        let err = load_existing(missing.to_str().unwrap(), "admin").err().expect("refused");
        assert!(err.to_string().contains("does not exist"), "{err}");
        assert!(!missing.exists());
        std::fs::create_dir_all(&missing).unwrap();
        let err = load_existing(missing.to_str().unwrap(), "admin").err().expect("refused");
        assert!(err.to_string().contains("no certificates"), "{err}");
        assert!(std::fs::read_dir(&missing).unwrap().next().is_none(), "nothing written");

        // The CA's own directory, and certificates `cert issue` wrote, load.
        let ca = base.join("ca");
        load_or_create(ca.to_str().unwrap(), "n1").unwrap();
        load_existing(ca.to_str().unwrap(), "admin").unwrap();
        let issued = base.join("issued");
        issue_cert_to_dir(ca.to_str().unwrap(), "admin", issued.to_str().unwrap()).unwrap();
        load_existing(issued.to_str().unwrap(), "admin").unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod validity_tests {
    use super::*;

    /// (not_before, not_after) of a PEM or DER certificate.
    fn validity(der: &[u8]) -> (time::OffsetDateTime, time::OffsetDateTime) {
        let (_, cert) = x509_parser::parse_x509_certificate(der).unwrap();
        (cert.validity().not_before.to_datetime(), cert.validity().not_after.to_datetime())
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stouter_{tag}_{}_{}", std::process::id(), rand::random::<u64>()))
    }

    #[test]
    fn certificates_are_valid_from_their_issue_date() {
        let dir = temp_dir("validity");
        let material = load_or_create(dir.to_str().unwrap(), "n1").unwrap();
        let now = time::OffsetDateTime::now_utc();
        let close = |a: time::OffsetDateTime, b: time::OffsetDateTime| (a - b).abs() < time::Duration::minutes(5);

        let (ca_from, ca_until) = validity(&material.ca_cert_der);
        assert!(close(ca_from, now - CLOCK_SKEW_ALLOWANCE) && close(ca_until, now + CA_VALIDITY));
        let (from, until) = validity(&material.node_cert_der);
        assert!(close(from, now - CLOCK_SKEW_ALLOWANCE) && close(until, now + NODE_CERT_VALIDITY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_certificates_never_outlive_their_ca() {
        // A CA with one year left.
        let dir = temp_dir("short_ca");
        std::fs::create_dir_all(&dir).unwrap();
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, "short-ca");
        let ca_until = time::OffsetDateTime::now_utc() + time::Duration::days(365);
        params.not_after = ca_until;
        let ca = params.self_signed(&key).unwrap();
        std::fs::write(dir.join("ca.crt"), ca.pem()).unwrap();
        std::fs::write(dir.join("ca.key"), key.serialize_pem()).unwrap();

        let out = dir.join("issued");
        issue_cert_to_dir(dir.to_str().unwrap(), "n2", out.to_str().unwrap()).unwrap();
        let pem = std::fs::read_to_string(out.join("node.crt")).unwrap();
        let (_, until) = validity(pem_to_der(&pem).unwrap().as_ref());
        assert_eq!(until.unix_timestamp(), ca_until.unix_timestamp());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
