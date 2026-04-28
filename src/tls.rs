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
    Plain(TcpStream),
    TlsServer(tokio_rustls::server::TlsStream<TcpStream>),
    TlsClient(tokio_rustls::client::TlsStream<TcpStream>),
}

impl MeshStream {
    pub fn from_plain(s: TcpStream) -> Self {
        MeshStream::Plain(s)
    }
}

impl AsyncRead for MeshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MeshStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MeshStream::TlsServer(s) => Pin::new(s).poll_read(cx, buf),
            MeshStream::TlsClient(s) => Pin::new(s).poll_read(cx, buf),
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
            MeshStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MeshStream::TlsServer(s) => Pin::new(s).poll_write(cx, buf),
            MeshStream::TlsClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MeshStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MeshStream::TlsServer(s) => Pin::new(s).poll_flush(cx),
            MeshStream::TlsClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MeshStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MeshStream::TlsServer(s) => Pin::new(s).poll_shutdown(cx),
            MeshStream::TlsClient(s) => Pin::new(s).poll_shutdown(cx),
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
            .with_client_cert_verifier(client_verifier)
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
        })
    }

    pub async fn accept(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self.acceptor.accept(stream).await.context("TLS accept")?;
        Ok(MeshStream::TlsServer(tls))
    }

    pub async fn connect(&self, stream: TcpStream) -> Result<MeshStream> {
        let tls = self
            .connector
            .connect(self.server_name.clone(), stream)
            .await
            .context("TLS connect")?;
        Ok(MeshStream::TlsClient(tls))
    }
}
