use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{self, pki_types::ServerName};

use crate::pki::PkiMaterial;

// ---------------------------------------------------------------------------
// MeshStream — unified plain / TLS stream
// ---------------------------------------------------------------------------

/// A TCP-backed stream that is either plain or mutually-authenticated TLS.
///
/// All variants are `Unpin` (TcpStream and TlsStream<TcpStream> are both
/// Unpin), so `AsyncRead` / `AsyncWrite` can be implemented via `get_mut()`.
pub enum MeshStream {
    TlsServer {
        stream: tokio_rustls::server::TlsStream<TcpStream>,
        peer_cn: Option<String>,
    },
    TlsClient {
        stream: tokio_rustls::client::TlsStream<TcpStream>,
        peer_cn: Option<String>,
    },
}

impl MeshStream {
    /// The Common Name of the peer's leaf certificate, if it has one: the
    /// client's on an accepted stream, the server's on a connected one.
    pub fn peer_cn(&self) -> Option<&str> {
        match self {
            MeshStream::TlsServer { peer_cn, .. } | MeshStream::TlsClient { peer_cn, .. } => peer_cn.as_deref(),
        }
    }
}

impl AsyncRead for MeshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
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
        match self.get_mut() {
            MeshStream::TlsServer { stream, .. } => Pin::new(stream).poll_write(cx, buf),
            MeshStream::TlsClient { stream, .. } => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
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
// TlsContext — acceptor + connector pair
// ---------------------------------------------------------------------------

pub struct TlsContext {
    acceptor: tokio_rustls::TlsAcceptor,
    connector: tokio_rustls::TlsConnector,
    server_name: ServerName<'static>,
    /// Checks a certificate chains to the cluster CA, as for a client
    /// connecting; also used to check who signed a config.
    cert_verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
}

impl TlsContext {
    pub fn new(pki: &PkiMaterial) -> Result<Self> {
        let mut root_store = rustls::RootCertStore::empty();
        root_store
            .add(pki.ca_cert_der.clone())
            .context("add CA cert to root store")?;
        let root_store = Arc::new(root_store);

        let cert_chain = vec![pki.node_cert_der.clone()];
        let private_key = pki.node_key.clone_key();

        // Server: require clients to present a cert signed by the cluster CA.
        let client_verifier =
            rustls::server::WebPkiClientVerifier::builder(root_store.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("build client verifier: {e}"))?;
        let server_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier.clone())
            .with_single_cert(cert_chain.clone(), private_key.clone_key())
            .context("build server TLS config")?;

        // Client: verify server cert against cluster CA; present own cert.
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(cert_chain, private_key)
            .context("build client TLS config")?;

        let server_name = ServerName::try_from(crate::pki::MESH_SERVER_NAME)
            .map_err(|e| anyhow::anyhow!("parse mesh server name: {e}"))?
            .to_owned();

        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server_config)),
            connector: tokio_rustls::TlsConnector::from(Arc::new(client_config)),
            server_name,
            cert_verifier: client_verifier,
        })
    }

    /// Check that `cert` (DER) was issued by the cluster CA and is currently
    /// valid; returns its Common Name.
    pub fn verify_cluster_cert(&self, cert: &rustls::pki_types::CertificateDer<'_>) -> Result<Option<String>> {
        self.cert_verifier
            .verify_client_cert(cert, &[], rustls::pki_types::UnixTime::now())
            .map_err(|e| anyhow::anyhow!("certificate not valid for this cluster: {e}"))?;
        Ok(leaf_cn(Some(std::slice::from_ref(cert))))
    }

    pub async fn accept(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self.acceptor.accept(stream).await.context("TLS accept")?;
        let peer_cn = peer_leaf_cn(&tls);
        Ok(MeshStream::TlsServer { stream: tls, peer_cn })
    }

    pub async fn connect(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self
            .connector
            .connect(self.server_name.clone(), stream)
            .await
            .context("TLS connect")?;
        let peer_cn = leaf_cn(tls.get_ref().1.peer_certificates());
        Ok(MeshStream::TlsClient { stream: tls, peer_cn })
    }
}

/// Extract the Common Name from the leaf cert of a freshly-accepted server-side
/// TLS stream. Returns `None` if the peer presented no cert (mTLS would have
/// already failed the handshake) or if the cert subject has no CN.
fn peer_leaf_cn(tls: &tokio_rustls::server::TlsStream<TcpStream>) -> Option<String> {
    leaf_cn(tls.get_ref().1.peer_certificates())
}

/// The Common Name of a DER certificate.
pub(crate) fn cert_cn(cert: &rustls::pki_types::CertificateDer<'_>) -> Option<String> {
    leaf_cn(Some(std::slice::from_ref(cert)))
}

/// The Common Name of the first (leaf) certificate in `certs`.
fn leaf_cn(certs: Option<&[rustls::pki_types::CertificateDer<'_>]>) -> Option<String> {
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
