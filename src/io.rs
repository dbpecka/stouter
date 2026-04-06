use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Buffer size for bidirectional proxy (64 KiB).
const BUF_SIZE: usize = 65536;

/// Close the tunnel if no data flows in either direction for this long.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// TCP keepalive idle time for long-lived connections.
const KEEPALIVE_TIME: Duration = Duration::from_secs(30);

/// Configure a TCP stream for long-lived tunnel use: enable `TCP_NODELAY`
/// and `SO_KEEPALIVE` (30 s idle time) so intermediary NATs/firewalls don't
/// silently drop idle connections.
pub fn configure_stream(stream: &TcpStream) {
    stream.set_nodelay(true).ok();
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE_TIME);
    let _ = sock.set_tcp_keepalive(&keepalive);
}

/// Bidirectional proxy with 64 KiB buffers and 5-minute idle timeout.
///
/// Copies data between `a` and `b` in both directions concurrently using
/// buffers 8x larger than tokio's default, reducing read syscalls for
/// typical HTTP payloads.
///
/// Each direction independently times out if no data is read for 5 minutes.
/// When either direction completes (EOF or idle timeout), both are torn down.
pub async fn proxy_bidirectional<A, B>(a: A, b: B) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let a_to_b = async {
        let mut buf = vec![0u8; BUF_SIZE];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, ar.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(e),
            };
            bw.write_all(&buf[..n]).await?;
        }
        bw.shutdown().await
    };

    let b_to_a = async {
        let mut buf = vec![0u8; BUF_SIZE];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, br.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(e),
            };
            aw.write_all(&buf[..n]).await?;
        }
        aw.shutdown().await
    };

    tokio::select! {
        result = a_to_b => result,
        result = b_to_a => result,
    }
}
