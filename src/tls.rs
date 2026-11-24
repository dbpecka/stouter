use std::collections::HashSet;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use arc_swap::ArcSwap;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{self, pki_types::ServerName, DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::pki::PkiMaterial;

// ---------------------------------------------------------------------------
// MeshStream — unified plain / TLS stream
// ---------------------------------------------------------------------------

/// A mutually-authenticated TLS stream, server or client side.
///
/// All variants are `Unpin` (TlsStream<TcpStream> is), so `AsyncRead` /
/// `AsyncWrite` can be implemented via `get_mut()`.
pub enum MeshStream {
    TlsServer {
        stream: tokio_rustls::server::TlsStream<TcpStream>,
        peer: Peer,
    },
    TlsClient {
        stream: tokio_rustls::client::TlsStream<TcpStream>,
        peer: Peer,
    },
}

/// Who is at the other end of a [`MeshStream`], by its certificate. Once
/// that certificate is revoked, the stream fails every read and write
/// (checked whenever the revocation list changes).
pub struct Peer {
    cn: Option<String>,
    fingerprint: Option<[u8; 32]>,
    revocations: Arc<Revocations>,
    /// The revocation list's generation last checked against.
    checked: u64,
}

impl Peer {
    fn new(certs: Option<&[CertificateDer<'_>]>, revocations: &Arc<Revocations>) -> Self {
        let leaf = certs.and_then(|certs| certs.first());
        Peer {
            cn: leaf_cn(certs),
            fingerprint: leaf.map(|cert| fingerprint(cert.as_ref())),
            revocations: revocations.clone(),
            checked: 0,
        }
    }

    fn revoked(&self) -> bool {
        match &self.fingerprint {
            Some(fp) => self.revocations.is_revoked(self.cn.as_deref(), fp),
            None => false,
        }
    }

    /// An error once the peer's certificate is revoked.
    fn check(&mut self) -> io::Result<()> {
        let generation = self.revocations.generation();
        if generation != self.checked {
            if self.revoked() {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "the peer's certificate is revoked"));
            }
            self.checked = generation;
        }
        Ok(())
    }
}

impl MeshStream {
    /// The local address of the underlying TCP connection.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            MeshStream::TlsServer { stream, .. } => stream.get_ref().0.local_addr(),
            MeshStream::TlsClient { stream, .. } => stream.get_ref().0.local_addr(),
        }
    }

    /// The Common Name of the peer's leaf certificate, if it has one: the
    /// client's on an accepted stream, the server's on a connected one.
    pub fn peer_cn(&self) -> Option<&str> {
        self.peer().cn.as_deref()
    }

    /// The SHA-256 fingerprint of the peer's leaf certificate.
    pub fn peer_fingerprint(&self) -> Option<[u8; 32]> {
        self.peer().fingerprint
    }

    /// Whether the peer's certificate has been revoked since it connected.
    pub fn peer_revoked(&self) -> bool {
        self.peer().revoked()
    }

    fn peer(&self) -> &Peer {
        match self {
            MeshStream::TlsServer { peer, .. } | MeshStream::TlsClient { peer, .. } => peer,
        }
    }

    fn check_peer(&mut self) -> io::Result<()> {
        match self {
            MeshStream::TlsServer { peer, .. } | MeshStream::TlsClient { peer, .. } => peer.check(),
        }
    }
}

impl AsyncRead for MeshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check_peer() {
            return Poll::Ready(Err(e));
        }
        match this {
            MeshStream::TlsServer { stream, .. } => Pin::new(stream).poll_read(cx, buf),
            MeshStream::TlsClient { stream, .. } => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MeshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(e) = this.check_peer() {
            return Poll::Ready(Err(e));
        }
        match this {
            MeshStream::TlsServer { stream, .. } => Pin::new(stream).poll_write(cx, buf),
            MeshStream::TlsClient { stream, .. } => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(e) = this.check_peer() {
            return Poll::Ready(Err(e));
        }
        match this {
            MeshStream::TlsServer { stream, .. } => Pin::new(stream).poll_flush(cx),
            MeshStream::TlsClient { stream, .. } => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MeshStream::TlsServer { stream, .. } => Pin::new(stream).poll_shutdown(cx),
            MeshStream::TlsClient { stream, .. } => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

// ---------------------------------------------------------------------------
// Revocations
// ---------------------------------------------------------------------------

/// SHA-256 of a certificate (DER).
pub fn fingerprint(cert: &[u8]) -> [u8; 32] {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, cert);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// [`fingerprint`] in lowercase hex, as the config's `revoked.certs` lists it.
pub fn fingerprint_hex(cert: &[u8]) -> String {
    fingerprint(cert).iter().map(|b| format!("{b:02x}")).collect()
}

/// The certificates this node no longer accepts (the config's `revoked`),
/// shared by every certificate check: TLS handshakes in both directions,
/// node record, beat and config signatures, and every open connection,
/// which fails once its peer's certificate is revoked.
#[derive(Default)]
pub struct Revocations {
    /// Bumped on every change, so a connection re-checks its peer only then.
    generation: AtomicU64,
    list: ArcSwap<RevokedSet>,
}

#[derive(Default, PartialEq)]
struct RevokedSet {
    nodes: HashSet<String>,
    certs: HashSet<[u8; 32]>,
}

impl Revocations {
    /// Take `revoked` as the list. Returns whether it changed.
    pub fn set(&self, revoked: &crate::config::Revoked) -> bool {
        let parse = |hex: &str| -> Option<[u8; 32]> {
            let hex = crate::config::normalize_fingerprint(hex).ok()?;
            let bytes: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)).collect::<Result<_, _>>().ok()?;
            bytes.try_into().ok()
        };
        let next = RevokedSet {
            nodes: revoked.nodes.iter().cloned().collect(),
            certs: revoked.certs.iter().filter_map(|c| parse(c)).collect(),
        };
        if **self.list.load() == next {
            return false;
        }
        self.list.store(Arc::new(next));
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether a certificate for `cn` with SHA-256 `fingerprint` is revoked.
    pub fn is_revoked(&self, cn: Option<&str>, fingerprint: &[u8; 32]) -> bool {
        let list = self.list.load();
        list.certs.contains(fingerprint) || cn.is_some_and(|cn| list.nodes.contains(cn))
    }

    /// Whether every certificate for `name` is revoked.
    pub fn name_revoked(&self, name: &str) -> bool {
        self.list.load().nodes.contains(name)
    }

    /// Whether certificate `cert` (hex DER, as signed records carry it) is
    /// revoked. One that isn't hex couldn't have verified: not revoked.
    pub fn cert_hex_revoked(&self, cert: &str) -> bool {
        // Checked for every beat in every sync: nothing to decode for while
        // nothing is revoked.
        if self.is_empty() || !cert.len().is_multiple_of(2) {
            return false;
        }
        let der: Option<Vec<u8>> = (0..cert.len()).step_by(2).map(|i| u8::from_str_radix(cert.get(i..i + 2)?, 16).ok()).collect();
        der.is_some_and(|der| self.cert_revoked(&der))
    }

    /// Whether certificate `cert` (DER) is revoked.
    pub fn cert_revoked(&self, cert: &[u8]) -> bool {
        if self.is_empty() {
            return false;
        }
        let cn = cert_cn(&CertificateDer::from(cert));
        self.is_revoked(cn.as_deref(), &fingerprint(cert))
    }

    fn is_empty(&self) -> bool {
        let list = self.list.load();
        list.nodes.is_empty() && list.certs.is_empty()
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.cert_revoked(cert.as_ref()) {
            return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::Revoked));
        }
        Ok(())
    }
}

/// The cluster CA's check of a client's certificate, then the revocation list.
struct ClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    revocations: Arc<Revocations>,
}

impl std::fmt::Debug for ClientVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientVerifier").finish_non_exhaustive()
    }
}

impl ClientCertVerifier for ClientVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self.inner.verify_client_cert(end_entity, intermediates, now)?;
        self.revocations.check(end_entity)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The cluster CA's check of a server's certificate, then the revocation list.
struct ServerVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    revocations: Arc<Revocations>,
}

impl std::fmt::Debug for ServerVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerVerifier").finish_non_exhaustive()
    }
}

impl ServerCertVerifier for ServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        self.revocations.check(end_entity)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

// ---------------------------------------------------------------------------
// TlsContext — acceptor + connector pair
// ---------------------------------------------------------------------------

pub struct TlsContext {
    acceptor: tokio_rustls::TlsAcceptor,
    connector: tokio_rustls::TlsConnector,
    server_name: ServerName<'static>,
    /// Checks a certificate chains to the cluster CA and isn't revoked, as
    /// for a client connecting; also used to check who signed a config.
    cert_verifier: Arc<dyn ClientCertVerifier>,
    revocations: Arc<Revocations>,
    identity: Arc<Identity>,
}

/// This node's certificate and key, presented in every new handshake as
/// server and as client, and replaced by a renewed one without a restart
/// (see `crate::renew`). Connections already open keep the one they began
/// with.
#[derive(Debug)]
struct Identity {
    current: ArcSwap<rustls::sign::CertifiedKey>,
}

impl Identity {
    fn certified(cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<Arc<rustls::sign::CertifiedKey>> {
        // Checks the key is the certificate's.
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let certified = rustls::sign::CertifiedKey::from_der(vec![cert], key.clone_key(), &provider)
            .map_err(|e| anyhow::anyhow!("certificate and key: {e}"))?;
        Ok(Arc::new(certified))
    }
}

impl rustls::server::ResolvesServerCert for Identity {
    fn resolve(&self, _: rustls::server::ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.current.load_full())
    }
}

impl rustls::client::ResolvesClientCert for Identity {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.current.load_full())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl TlsContext {
    pub fn new(pki: &PkiMaterial) -> Result<Self> {
        let mut root_store = rustls::RootCertStore::empty();
        root_store
            .add(pki.ca_cert_der.clone())
            .context("add CA cert to root store")?;
        let root_store = Arc::new(root_store);
        let revocations = Arc::new(Revocations::default());

        let identity = Arc::new(Identity {
            current: ArcSwap::new(Identity::certified(pki.node_cert_der.clone(), &pki.node_key)?),
        });

        // Server: require clients to present a cert signed by the cluster CA.
        let client_verifier: Arc<dyn ClientCertVerifier> = Arc::new(ClientVerifier {
            inner: rustls::server::WebPkiClientVerifier::builder(root_store.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("build client verifier: {e}"))?,
            revocations: revocations.clone(),
        });
        let mut server_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier.clone())
            .with_cert_resolver(identity.clone());
        // No session resumption, either way: a resumed session skips the
        // certificate checks, and carries the certificate of the handshake
        // it resumes, whatever has been revoked, has expired or was renewed
        // since. Mesh connections are long-lived and pooled, so a full
        // handshake each costs little.
        server_config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        server_config.send_tls13_tickets = 0;

        // Client: verify server cert against cluster CA; present own cert.
        let server_verifier = Arc::new(ServerVerifier {
            inner: rustls::client::WebPkiServerVerifier::builder(root_store)
                .build()
                .map_err(|e| anyhow::anyhow!("build server verifier: {e}"))?,
            revocations: revocations.clone(),
        });
        let mut client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(server_verifier)
            .with_client_cert_resolver(identity.clone());
        client_config.resumption = rustls::client::Resumption::disabled();

        let server_name = ServerName::try_from(crate::pki::MESH_SERVER_NAME)
            .map_err(|e| anyhow::anyhow!("parse mesh server name: {e}"))?
            .to_owned();

        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server_config)),
            connector: tokio_rustls::TlsConnector::from(Arc::new(client_config)),
            server_name,
            cert_verifier: client_verifier,
            revocations,
            identity,
        })
    }

    /// Present `cert` (with `key`) in new handshakes from now on: a renewed
    /// certificate. Refused unless it is from the cluster CA, not revoked,
    /// and the key is its own.
    pub fn rotate(&self, cert: CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<()> {
        self.verify_cluster_cert(&cert)?;
        self.identity.current.store(Identity::certified(cert, key)?);
        Ok(())
    }

    /// Whether `cert` with `key` would do for [`Self::rotate`]: from the
    /// cluster CA, not revoked, and the key its own.
    pub fn check_identity(&self, cert: &CertificateDer<'static>, key: &PrivateKeyDer<'_>) -> Result<()> {
        self.verify_cluster_cert(cert)?;
        Identity::certified(cert.clone(), key).map(|_| ())
    }

    /// This node's certificate now.
    pub fn own_cert(&self) -> CertificateDer<'static> {
        self.identity.current.load().cert[0].clone()
    }

    /// The revocation list every check made with this context uses.
    pub fn revocations(&self) -> &Arc<Revocations> {
        &self.revocations
    }

    /// Whether this node's own certificate is revoked.
    pub fn own_cert_revoked(&self) -> bool {
        self.revocations.cert_revoked(self.own_cert().as_ref())
    }

    /// Check that `cert` (DER) was issued by the cluster CA and is currently
    /// valid; returns its Common Name.
    pub fn verify_cluster_cert(&self, cert: &CertificateDer<'_>) -> Result<Option<String>> {
        self.verify_cluster_cert_at(cert, UnixTime::now())
    }

    /// [`Self::verify_cluster_cert`] as of `at`: whether `cert` was valid
    /// then. Revocation is still checked as of now.
    pub fn verify_cluster_cert_at(&self, cert: &CertificateDer<'_>, at: UnixTime) -> Result<Option<String>> {
        self.cert_verifier
            .verify_client_cert(cert, &[], at)
            .map_err(|e| anyhow::anyhow!("certificate not valid for this cluster: {e}"))?;
        Ok(leaf_cn(Some(std::slice::from_ref(cert))))
    }

    pub async fn accept(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self.acceptor.accept(stream).await.context("TLS accept")?;
        let peer = Peer::new(tls.get_ref().1.peer_certificates(), &self.revocations);
        Ok(MeshStream::TlsServer { stream: tls, peer })
    }

    pub async fn connect(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self
            .connector
            .connect(self.server_name.clone(), stream)
            .await
            .context("TLS connect")?;
        let peer = Peer::new(tls.get_ref().1.peer_certificates(), &self.revocations);
        Ok(MeshStream::TlsClient { stream: tls, peer })
    }
}

/// The Common Name of a DER certificate.
pub(crate) fn cert_cn(cert: &CertificateDer<'_>) -> Option<String> {
    leaf_cn(Some(std::slice::from_ref(cert)))
}

/// The Common Name of the first (leaf) certificate in `certs`.
fn leaf_cn(certs: Option<&[CertificateDer<'_>]>) -> Option<String> {
    let leaf = certs?.first()?;
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).ok()?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(|s| s.to_string());
    cn
}
