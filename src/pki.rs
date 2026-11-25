use std::path::Path;

use anyhow::{bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tracing::info;

/// The fixed DNS SAN stamped into every node cert so WebPki hostname
/// verification can be used without per-node cert customisation.
pub const MESH_SERVER_NAME: &str = "stouter-node";

/// How long a node certificate is valid from when it is issued (but never
/// past the CA's own expiry), unless the CA node's `certLifetime` says
/// otherwise.
pub const DEFAULT_CERT_LIFETIME: time::Duration = time::Duration::days(10 * 365);

/// Shortest `certLifetime`: renewal is checked every tenth of it.
const MIN_CERT_LIFETIME: time::Duration = time::Duration::minutes(1);

/// How long a newly generated cluster CA is valid.
const CA_VALIDITY: time::Duration = time::Duration::days(20 * 365);

/// Certificates start this far in the past, so a peer whose clock runs a
/// little behind still accepts one issued just now.
pub const CLOCK_SKEW_ALLOWANCE: time::Duration = time::Duration::days(1);

/// Validity window for a node certificate signed by `ca`: from now (less
/// the skew allowance) for `lifetime`, ending no later than the CA does,
/// since a certificate can't outlive the CA that vouches for it.
fn node_cert_validity(ca: &rcgen::Certificate, lifetime: time::Duration) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (now - CLOCK_SKEW_ALLOWANCE, (now + lifetime).min(ca.params().not_after))
}

/// A certificate lifetime such as `30d`, `12h`, `45m` or `90s`.
pub fn parse_lifetime(text: &str) -> std::result::Result<time::Duration, String> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len()));
    let n: i64 = number.parse().map_err(|_| format!("certLifetime '{text}' is not a number and a unit (30d, 12h, 45m, 90s)"))?;
    let unit_secs: i64 = match unit {
        "d" => 86_400,
        "h" => 3_600,
        "m" => 60,
        "s" => 1,
        _ => return Err(format!("certLifetime '{text}' needs a unit: d, h, m or s (as in 30d)")),
    };
    // Multiplied with a check: `time::Duration::days(n)` panics on overflow.
    let lifetime = n
        .checked_mul(unit_secs)
        .map(time::Duration::seconds)
        .ok_or_else(|| format!("certLifetime '{text}' is longer than the CA lasts (20 years)"))?;
    if lifetime < MIN_CERT_LIFETIME {
        return Err(format!("certLifetime '{text}' is shorter than a minute"));
    }
    if lifetime > CA_VALIDITY {
        return Err(format!("certLifetime '{text}' is longer than the CA lasts (20 years)"));
    }
    Ok(lifetime)
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
#[cfg(test)]
pub fn load_or_create(tls_dir: &str, node_id: &str) -> Result<PkiMaterial> {
    load_or_create_with(tls_dir, node_id, DEFAULT_CERT_LIFETIME)
}

/// [`load_or_create`], issuing this node's certificate (as the CA node)
/// for `lifetime`.
pub fn load_or_create_with(tls_dir: &str, node_id: &str, lifetime: time::Duration) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create TLS directory {tls_dir}"))?;
    finish_renewal(dir)?;
    load_dir(tls_dir, node_id, lifetime)
}

/// The certificates in `tls_dir` (see [`load_or_create`]), changing nothing
/// there but a new CA in an empty directory.
fn load_dir(tls_dir: &str, node_id: &str, lifetime: time::Duration) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);

    let ca_cert_path  = dir.join("ca.crt");
    let ca_key_path   = dir.join("ca.key");
    let node_cert_path = dir.join("node.crt");
    let node_key_path  = dir.join("node.key");

    if ca_cert_path.exists() && ca_key_path.exists() {
        let (ca_cert_der, ca_cert_rcgen, ca_key) = load_ca(&ca_cert_path, &ca_key_path)?;
        let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key, lifetime)?;
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
        let (node_cert_der, node_key) = generate_node_cert(node_id, &ca_cert_rcgen, &ca_key, lifetime)?;
        Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
    }
}

/// Like [`load_or_create`], but never creates anything: for the CLI. A
/// directory with no certificates there is a mistyped `--cert-dir` (or a
/// node's `tlsDir` before its daemon first ran), not a new cluster, and a
/// CA made up on the spot would only fail the handshake later, obscurely.
///
/// Nor does it finish a renewal a crash cut short (see [`finish_renewal`]):
/// only the daemon does, at startup. A CLI command doing it could catch the
/// daemon's own save between its two renames, discard the staged key as
/// left over, and leave the new certificate beside the old key.
pub fn load_existing(tls_dir: &str, node_id: &str) -> Result<PkiMaterial> {
    let dir = Path::new(tls_dir);
    let has = |name: &str| dir.join(name).is_file();
    if has("ca.crt") && (has("ca.key") || (has("node.crt") && has("node.key"))) {
        return load_dir(tls_dir, node_id, DEFAULT_CERT_LIFETIME);
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
#[cfg(test)]
pub fn issue_cert_to_dir(tls_dir: &str, node_id: &str, out_dir: &str) -> Result<()> {
    issue_cert_to_dir_with(tls_dir, node_id, out_dir, DEFAULT_CERT_LIFETIME)
}

/// [`issue_cert_to_dir`], for `lifetime`.
pub fn issue_cert_to_dir_with(tls_dir: &str, node_id: &str, out_dir: &str, lifetime: time::Duration) -> Result<()> {
    let dir = Path::new(tls_dir);
    let ca_cert_path = dir.join("ca.crt");
    let ca_key_path  = dir.join("ca.key");

    if !ca_cert_path.exists() || !ca_key_path.exists() {
        bail!(
            "{tls_dir} holds no CA (ca.crt and ca.key): run this on the CA node (the one whose tlsDir holds ca.key), \
             with its config"
        );
    }
    let out = Path::new(out_dir);
    // Never beside a CA's key (the CA's own tlsDir, say): copying the
    // files to the node would copy that too, and whoever holds it can issue
    // certificates for any name.
    if out.join("ca.key").exists() {
        bail!(
            "{out_dir} holds ca.key (a CA's own directory): the files for '{node_id}' would lie beside it, and \
             copying them could copy it along. Write them to a directory of their own (--out {node_id}-tls), and \
             copy ca.crt, node.crt and node.key from there"
        );
    }
    // Nor over another node's certificate. Over this node's own is how it
    // is replaced by hand.
    if let Ok(existing) = read_cert(&out.join("node.crt").to_string_lossy()) {
        if let Some(cn) = crate::tls::cert_cn(&existing).filter(|cn| cn != node_id) {
            bail!("{out_dir} holds the certificate of '{cn}': not writing '{node_id}''s over it; use another directory");
        }
    }

    let (_, ca_cert_rcgen, ca_key) = load_ca(&ca_cert_path, &ca_key_path)?;

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate node key")?;

    let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()])
        .context("node cert params")?;
    params.distinguished_name.push(DnType::CommonName, node_id);
    (params.not_before, params.not_after) = node_cert_validity(&ca_cert_rcgen, lifetime);

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
                tracing::warn!(
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
    // Both from the same CA: a node started with an empty tlsDir makes a
    // CA of its own, and the cluster's ca.crt copied in afterwards would
    // otherwise be taken with that key, every certificate made from the
    // pair refused by peers (and nothing said).
    if !holds_key_of(&der, &key.public_key_der()) {
        bail!(
            "{} isn't the key of {}: they come from different CAs (most likely the cluster's ca.crt was copied over \
             one this node made itself, started before its certificates were in place). A node joining a cluster \
             needs ca.crt, node.crt and node.key from `stouter cert issue`, and no ca.key: remove {}",
            key_path.display(),
            cert_path.display(),
            key_path.display()
        );
    }
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
    lifetime: time::Duration,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate node key")?;

    let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()])
        .context("node cert params")?;
    params.distinguished_name.push(DnType::CommonName, node_id);
    (params.not_before, params.not_after) = node_cert_validity(ca_cert, lifetime);

    let cert = params.signed_by(&key, ca_cert, ca_key)
        .context("sign node cert with CA")?;

    let der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));

    Ok((der, key_der))
}

// ---------------------------------------------------------------------------
// Renewal
// ---------------------------------------------------------------------------

/// The cluster CA, on a node whose `tlsDir` holds `ca.key`: issues renewed
/// certificates (see `crate::renew`).
pub struct Issuer {
    ca_cert: rcgen::Certificate,
    ca_key: KeyPair,
    lifetime: time::Duration,
}

impl Issuer {
    /// The CA in `tls_dir`, if this node holds its key.
    pub fn load(tls_dir: &str, lifetime: time::Duration) -> Result<Option<Self>> {
        let dir = Path::new(tls_dir);
        let (cert_path, key_path) = (dir.join("ca.crt"), dir.join("ca.key"));
        if !key_path.exists() {
            return Ok(None);
        }
        let (_, ca_cert, ca_key) = load_ca(&cert_path, &key_path)?;
        Ok(Some(Self { ca_cert, ca_key, lifetime }))
    }

    /// A certificate for `node_id` on the key in `csr` (DER), which the
    /// request's own signature shows the requester holds. Only the key is
    /// taken from the request: the name is `node_id` (whom the requester's
    /// current certificate names), and the rest is what any node
    /// certificate has.
    pub fn issue_for_csr(&self, node_id: &str, csr: &[u8]) -> Result<CertificateDer<'static>> {
        let csr = rcgen::CertificateSigningRequestParams::from_der(&csr.to_vec().into())
            .map_err(|e| anyhow::anyhow!("certificate request: {e}"))?;
        if rcgen::PublicKeyData::algorithm(&csr.public_key) != &rcgen::PKCS_ECDSA_P256_SHA256 {
            bail!("certificate request: the key must be ECDSA P-256");
        }
        let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()]).context("node cert params")?;
        params.distinguished_name.push(DnType::CommonName, node_id);
        (params.not_before, params.not_after) = node_cert_validity(&self.ca_cert, self.lifetime);
        let csr = rcgen::CertificateSigningRequestParams { params, public_key: csr.public_key };
        let cert = csr.signed_by(&self.ca_cert, &self.ca_key).context("sign renewed cert with CA")?;
        Ok(CertificateDer::from(cert.der().to_vec()))
    }

    /// A new certificate and key for `node_id` (the CA node's own).
    pub fn issue_new(&self, node_id: &str) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
        generate_node_cert(node_id, &self.ca_cert, &self.ca_key, self.lifetime)
    }
}

/// A new key, and a request (CSR, DER) for a certificate on it, signed
/// with it: for renewal by a node without the CA's key.
pub fn new_csr(node_id: &str) -> Result<(Vec<u8>, PrivateKeyDer<'static>)> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).context("generate node key")?;
    let mut params = CertificateParams::new(vec![MESH_SERVER_NAME.to_string()]).context("csr params")?;
    params.distinguished_name.push(DnType::CommonName, node_id);
    let csr = params.serialize_request(&key).context("make certificate request")?;
    Ok((csr.der().to_vec(), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()))))
}

/// When `cert` (DER) is valid from and until, in Unix seconds.
pub fn validity(cert: &[u8]) -> Option<(i64, i64)> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert).ok()?;
    Some((cert.validity().not_before.timestamp(), cert.validity().not_after.timestamp()))
}

/// Write a renewed certificate and key to `tls_dir` (as `node.crt` and
/// `node.key`), for the next start. Both are written in full first, then
/// staged beside their final names as `*.next` (the key first), then
/// renamed into place (the key first); [`finish_renewal`] completes or
/// discards whatever a crash part way leaves, so the two never end up
/// mismatched.
pub fn save_renewed(tls_dir: &str, cert: &CertificateDer<'_>, key: &PrivateKeyDer<'_>) -> Result<()> {
    let dir = Path::new(tls_dir);
    let cert_pem = to_pem("CERTIFICATE", cert.as_ref());
    let key_pem = match key {
        PrivateKeyDer::Pkcs8(der) => to_pem("PRIVATE KEY", der.secret_pkcs8_der()),
        _ => bail!("renewed key is not PKCS#8"),
    };
    let (key_tmp, cert_tmp) = (dir.join("node.key.tmp"), dir.join("node.crt.tmp"));
    write_private_key(&key_tmp, &key_pem).with_context(|| format!("write {}", key_tmp.display()))?;
    write_synced(&cert_tmp, &cert_pem).with_context(|| format!("write {}", cert_tmp.display()))?;
    std::fs::rename(&key_tmp, dir.join("node.key.next")).context("stage node.key.next")?;
    std::fs::rename(&cert_tmp, dir.join("node.crt.next")).context("stage node.crt.next")?;
    finish_renewal(dir)
}

/// Write `text` to `path` and flush it to disk, through the handle that
/// wrote it (Windows flushes only through one with write access).
fn write_synced(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// `der` as a PEM block labelled `label`.
fn to_pem(label: &str, der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::with_capacity(der.len().div_ceil(3) * 4);
    for chunk in der.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            b64.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    let mut pem = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

/// Finish a [`save_renewed`] a crash cut short, from the staged files
/// left (`node.key.next` is staged, and put in place, before
/// `node.crt.next`):
/// - both: staged in full; put both in place, the key first;
/// - the certificate alone: the key is already in place; put it there too;
/// - the key alone: its certificate was never staged; discard it, keeping
///   the pair in place (putting it there would pair it with the old
///   certificate, and the node couldn't start).
fn finish_renewal(dir: &Path) -> Result<()> {
    let (key_next, cert_next) = (dir.join("node.key.next"), dir.join("node.crt.next"));
    let put = |next: &Path, name: &str| {
        std::fs::rename(next, dir.join(name)).with_context(|| format!("put {} in place", next.display()))
    };
    match (key_next.exists(), cert_next.exists()) {
        (true, true) => {
            put(&key_next, "node.key")?;
            put(&cert_next, "node.crt")
        }
        (false, true) => put(&cert_next, "node.crt"),
        (true, false) => {
            tracing::warn!("discarding {}: a renewal stopped before its certificate was saved", key_next.display());
            std::fs::remove_file(&key_next).with_context(|| format!("remove {}", key_next.display()))
        }
        (false, false) => Ok(()),
    }
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
    let public = KeyPair::from_pem(&node_key_pem).map(|key| key.public_key_der());
    if public.is_ok_and(|public| !holds_key_of(&node_cert_der, &public)) {
        bail!(
            "{} isn't the key of {}: copy both from the same `stouter cert issue`",
            node_key_path.display(),
            node_cert_path.display()
        );
    }

    info!("loaded pre-issued certificate ← {}", node_cert_path.display());
    Ok(PkiMaterial { ca_cert_der, node_cert_der, node_key })
}

// ---------------------------------------------------------------------------
// PEM decode helpers
// ---------------------------------------------------------------------------

/// Whether certificate `cert` (DER) is for the public key `public` (DER
/// SubjectPublicKeyInfo).
fn holds_key_of(cert: &[u8], public: &[u8]) -> bool {
    x509_parser::parse_x509_certificate(cert).is_ok_and(|(_, cert)| cert.public_key().raw == public)
}

/// Whether `tls_dir` holds any certificate or key of ours.
pub fn holds_certificates(tls_dir: &str) -> bool {
    ["ca.crt", "ca.key", "node.crt", "node.key"].iter().any(|name| Path::new(tls_dir).join(name).exists())
}

/// What would stop a daemon starting with the certificates in `tls_dir`
/// (some of the files but not a set, a key that isn't its certificate's),
/// changing nothing. None at all is fine: the daemon makes a new CA.
pub fn check_dir(tls_dir: &str, node_id: &str) -> Result<()> {
    if !holds_certificates(tls_dir) {
        return Ok(());
    }
    let dir = Path::new(tls_dir);
    // A renewal a crash cut short, which the daemon finishes as it starts
    // (see `finish_renewal`): finished on a copy, as the daemon would,
    // without touching the directory (read-only, in deploy.sh's check).
    let material = if dir.join("node.key.next").exists() || dir.join("node.crt.next").exists() {
        let copy = std::env::temp_dir().join(format!("stouter_check_{}_{}", std::process::id(), rand::random::<u64>()));
        let checked = (|| {
            std::fs::create_dir_all(&copy)?;
            for name in ["ca.crt", "ca.key", "node.crt", "node.key", "node.key.next", "node.crt.next"] {
                if dir.join(name).exists() {
                    std::fs::copy(dir.join(name), copy.join(name)).with_context(|| format!("copy {name}"))?;
                }
            }
            finish_renewal(&copy)?;
            load_dir(copy.to_str().unwrap_or_default(), node_id, DEFAULT_CERT_LIFETIME)
        })();
        let _ = std::fs::remove_dir_all(&copy);
        checked?
    } else {
        load_dir(tls_dir, node_id, DEFAULT_CERT_LIFETIME)?
    };
    // And taken as the daemon takes them, which checks the key against its
    // certificate whatever the key's format (the check above reads only
    // PKCS#8 keys).
    crate::tls::TlsContext::new(&material).context("certificate and key")?;
    Ok(())
}

/// The (first) certificate in PEM file `path`.
pub fn read_cert(path: &str) -> Result<CertificateDer<'static>> {
    let pem = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    pem_to_der(&pem).with_context(|| path.to_string())
}

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

    #[test]
    fn a_huge_cert_lifetime_is_refused_not_a_crash() {
        for huge in ["999999999999999d", "9223372036854775807h", "9223372036854775807s"] {
            assert!(parse_lifetime(huge).is_err(), "{huge}");
        }
        assert_eq!(parse_lifetime("30d").unwrap(), time::Duration::days(30));
    }

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
        assert!(close(from, now - CLOCK_SKEW_ALLOWANCE) && close(until, now + DEFAULT_CERT_LIFETIME));
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

    /// A node started before its certificates were in place made a CA of
    /// its own; the cluster's files were copied in afterwards. Its ca.key
    /// and the cluster's ca.crt don't go together: refused, saying why,
    /// rather than taken (every certificate made from them refused by peers,
    /// and nothing said).
    #[test]
    fn a_key_that_isnt_its_certificates_is_refused() {
        let dir = temp_dir("mismatch");
        let (cluster, joining) = (dir.join("cluster"), dir.join("joining"));
        load_or_create(cluster.to_str().unwrap(), "ca-node").unwrap();
        load_or_create(joining.to_str().unwrap(), "n2").unwrap(); // its own CA
        issue_cert_to_dir(cluster.to_str().unwrap(), "n2", dir.join("issued").to_str().unwrap()).unwrap();
        for file in ["ca.crt", "node.crt", "node.key"] {
            std::fs::copy(dir.join("issued").join(file), joining.join(file)).unwrap();
        }
        let joining_s = joining.to_str().unwrap();
        let Err(err) = load_or_create(joining_s, "n2") else { panic!("taken") };
        assert!(format!("{err:#}").contains("isn't the key of"), "{err:#}");
        assert!(check_dir(joining_s, "n2").is_err(), "and `check --local` says so");
        // Without the stray ca.key, the issued set is taken.
        std::fs::remove_file(joining.join("ca.key")).unwrap();
        assert!(load_or_create(joining_s, "n2").is_ok());

        // An issued certificate with another one's key: refused too.
        issue_cert_to_dir(cluster.to_str().unwrap(), "n3", dir.join("other").to_str().unwrap()).unwrap();
        std::fs::copy(dir.join("other").join("node.key"), joining.join("node.key")).unwrap();
        let Err(err) = load_or_create(joining_s, "n2") else { panic!("taken") };
        assert!(format!("{err:#}").contains("isn't the key of"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_dir_finds_what_would_stop_the_daemon_and_changes_nothing() {
        let dir = temp_dir("check_dir");
        let s = dir.to_str().unwrap();
        assert!(check_dir(s, "n1").is_ok(), "none at all: the daemon makes a new CA");
        assert!(!dir.exists(), "and checking made nothing");
        load_or_create(s, "n1").unwrap();
        assert!(check_dir(s, "n1").is_ok());
        std::fs::remove_file(dir.join("ca.key")).unwrap();
        let err = check_dir(s, "n1").unwrap_err();
        assert!(format!("{err:#}").contains("not a complete set"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A renewal cut short between its two renames (the new key in place,
    /// the new certificate still staged), which the daemon finishes as it
    /// starts: `check --local` takes it too, and changes nothing.
    #[test]
    fn check_dir_takes_a_renewal_the_daemon_would_finish() {
        let dir = temp_dir("check_midrenew");
        let (ca, node) = (dir.join("ca"), dir.join("node"));
        load_or_create(ca.to_str().unwrap(), "ca-node").unwrap();
        issue_cert_to_dir(ca.to_str().unwrap(), "n1", node.to_str().unwrap()).unwrap();
        // A second issue, as a renewal stages it: the key already moved in.
        let staged = dir.join("staged");
        issue_cert_to_dir(ca.to_str().unwrap(), "n1", staged.to_str().unwrap()).unwrap();
        std::fs::copy(staged.join("node.key"), node.join("node.key")).unwrap();
        std::fs::copy(staged.join("node.crt"), node.join("node.crt.next")).unwrap();
        check_dir(node.to_str().unwrap(), "n1").expect("the daemon would start");
        assert!(node.join("node.crt.next").exists(), "nothing changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A node.key in another format (SEC1, here) that isn't node.crt's:
    /// refused by `check --local`, as the daemon would refuse it.
    #[test]
    fn check_dir_refuses_a_mismatched_key_whatever_its_format() {
        let dir = temp_dir("check_sec1");
        let (ca, node) = (dir.join("ca"), dir.join("node"));
        load_or_create(ca.to_str().unwrap(), "ca-node").unwrap();
        issue_cert_to_dir(ca.to_str().unwrap(), "n1", node.to_str().unwrap()).unwrap();
        use aws_lc_rs::encoding::AsDer;
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        // SEC1 ("EC PRIVATE KEY") form of another key.
        let pkcs8 = other.serialize_der();
        let sec1 = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(&aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8)
            .unwrap()
            .private_key()
            .as_der()
            .unwrap();
        let pem = format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
            base64_lines(sec1.as_ref())
        );
        std::fs::write(node.join("node.key"), pem).unwrap();
        assert!(check_dir(node.to_str().unwrap(), "n1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn base64_lines(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out.as_bytes().chunks(64).map(|l| String::from_utf8_lossy(l).into_owned()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn issued_files_never_go_beside_a_cas_key_or_over_another_nodes() {
        let dir = temp_dir("issue_out");
        let ca = dir.join("ca");
        let ca_s = ca.to_str().unwrap();
        load_or_create(ca_s, "ca-node").unwrap();
        // The README once said `--out` the tlsDir: on the CA node, its own.
        let err = issue_cert_to_dir(ca_s, "host2", ca_s).unwrap_err();
        assert!(format!("{err:#}").contains("holds ca.key"), "{err:#}");
        assert!(!ca.join("node.key").exists(), "nothing written");
        // Over another node's certificate: no. Over the same node's (how
        // one is replaced by hand): yes.
        let out = dir.join("host2-tls");
        let out_s = out.to_str().unwrap();
        issue_cert_to_dir(ca_s, "host2", out_s).unwrap();
        let err = issue_cert_to_dir(ca_s, "host3", out_s).unwrap_err();
        assert!(format!("{err:#}").contains("holds the certificate of 'host2'"), "{err:#}");
        issue_cert_to_dir(ca_s, "host2", out_s).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod renewal_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stouter_renew_{tag}_{}_{}", std::process::id(), rand::random::<u64>()))
    }

    #[test]
    fn a_renewal_is_issued_for_the_requester_on_its_new_key() {
        let dir = temp_dir("csr");
        let ca_dir = dir.to_str().unwrap();
        load_or_create(ca_dir, "ca-node").unwrap();
        let issuer = Issuer::load(ca_dir, time::Duration::hours(1)).unwrap().expect("holds ca.key");

        // The request asks for another name; the certificate names the
        // node whose current certificate made the request.
        let (csr, key) = new_csr("someone-else").unwrap();
        let cert = issuer.issue_for_csr("n1", &csr).unwrap();
        assert_eq!(crate::tls::cert_cn(&cert).as_deref(), Some("n1"));
        // On the requester's new key, and valid for this cluster.
        let material = PkiMaterial { ca_cert_der: load_or_create(ca_dir, "x").unwrap().ca_cert_der, node_cert_der: cert.clone(), node_key: key };
        let ctx = crate::tls::TlsContext::new(&material).expect("the key is the certificate's");
        assert_eq!(ctx.verify_cluster_cert(&cert).unwrap().as_deref(), Some("n1"));
        // For the lifetime asked (plus the skew allowance before now).
        let (from, until) = validity(cert.as_ref()).unwrap();
        let lifetime = until - from - CLOCK_SKEW_ALLOWANCE.whole_seconds();
        assert!((3590..=3610).contains(&lifetime), "{lifetime}");

        assert!(issuer.issue_for_csr("n1", b"not a csr").is_err());
        assert!(Issuer::load(temp_dir("none").to_str().unwrap(), DEFAULT_CERT_LIFETIME).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renewed_pair_is_never_left_mismatched() {
        let dir = temp_dir("save");
        let ca_dir = dir.join("ca");
        let node_dir = dir.join("n1");
        load_or_create(ca_dir.to_str().unwrap(), "ca-node").unwrap();
        issue_cert_to_dir(ca_dir.to_str().unwrap(), "n1", node_dir.to_str().unwrap()).unwrap();
        let node = node_dir.to_str().unwrap();
        let issuer = Issuer::load(ca_dir.to_str().unwrap(), DEFAULT_CERT_LIFETIME).unwrap().unwrap();

        let (cert, key) = issuer.issue_new("n1").unwrap();
        save_renewed(node, &cert, &key).unwrap();
        let loaded = load_existing(node, "n1").unwrap();
        assert_eq!(loaded.node_cert_der, cert, "the renewed pair is what loads");
        crate::tls::TlsContext::new(&loaded).expect("and its key matches");

        // A crash after the key was put in place but before the cert: the
        // next load finishes the job.
        let (cert2, key2) = issuer.issue_new("n1").unwrap();
        save_renewed(node, &cert2, &key2).unwrap();
        let (cert3, key3) = issuer.issue_new("n1").unwrap();
        std::fs::write(node_dir.join("node.crt.next"), to_pem("CERTIFICATE", cert3.as_ref())).unwrap();
        let PrivateKeyDer::Pkcs8(der) = &key3 else { unreachable!() };
        std::fs::write(node_dir.join("node.key"), to_pem("PRIVATE KEY", der.secret_pkcs8_der())).unwrap();
        // A CLI command leaves it to the daemon (it could be catching the
        // daemon's own save half way)...
        let _ = load_existing(node, "n1");
        assert!(node_dir.join("node.crt.next").exists(), "the CLI touches nothing");
        // ...which finishes it at startup.
        let loaded = load_or_create(node, "n1").unwrap();
        assert_eq!(loaded.node_cert_der, cert3);
        crate::tls::TlsContext::new(&loaded).expect("key and cert match");
        assert!(!node_dir.join("node.crt.next").exists());

        // A save stopped after staging the key, before the certificate (a
        // failed write, a crash): the key is discarded, and the pair in
        // place still loads and matches.
        let (_, key4) = issuer.issue_new("n1").unwrap();
        let PrivateKeyDer::Pkcs8(der) = &key4 else { unreachable!() };
        std::fs::write(node_dir.join("node.key.next"), to_pem("PRIVATE KEY", der.secret_pkcs8_der())).unwrap();
        // The CLI, catching a save between its two renames, mustn't take
        // the staged key for a leftover.
        let loaded = load_existing(node, "n1").unwrap();
        assert!(node_dir.join("node.key.next").exists(), "the CLI discards nothing");
        assert_eq!(loaded.node_cert_der, cert3);
        let loaded = load_or_create(node, "n1").unwrap();
        assert_eq!(loaded.node_cert_der, cert3);
        crate::tls::TlsContext::new(&loaded).expect("still a matching pair");
        assert!(!node_dir.join("node.key.next").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lifetimes_read_with_their_unit() {
        assert_eq!(parse_lifetime("30d").unwrap(), time::Duration::days(30));
        assert_eq!(parse_lifetime("12h").unwrap(), time::Duration::hours(12));
        assert_eq!(parse_lifetime("90s").unwrap(), time::Duration::seconds(90));
        assert!(parse_lifetime("30").is_err());
        assert!(parse_lifetime("10s").is_err(), "under a minute");
        assert!(parse_lifetime("30y").is_err());
        assert!(parse_lifetime("100000d").is_err(), "past the CA's own life");
    }
}
