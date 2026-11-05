use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Pacing for an accept loop after `accept` fails. Such failures mostly pass
/// on their own (out of file descriptors until connections close, a
/// connection reset before it was taken), so the loop waits a little,
/// longer each time, and carries on. Ending it instead would take the
/// listener, or the whole daemon, down over a moment's shortage, which
/// anyone able to open connections could cause on purpose.
#[derive(Default)]
pub struct AcceptBackoff {
    failures: u32,
}

impl AcceptBackoff {
    /// After a failed accept: log it (at warn the first time and as the
    /// streak doubles), then wait 10ms, doubling to about a second. Returns
    /// false if `stop` was cancelled meanwhile.
    pub async fn failed(&mut self, what: &str, err: &io::Error, stop: &tokio_util::sync::CancellationToken) -> bool {
        self.failures = self.failures.saturating_add(1);
        if self.failures.is_power_of_two() {
            tracing::warn!("{what}: accept failed ({err}), {} in a row; retrying", self.failures);
        } else {
            tracing::debug!("{what}: accept failed ({err}); retrying");
        }
        let wait = Duration::from_millis(5 << self.failures.min(8));
        tokio::select! {
            _ = tokio::time::sleep(wait) => true,
            _ = stop.cancelled() => false,
        }
    }

    /// After a successful accept.
    pub fn succeeded(&mut self, what: &str) {
        if self.failures > 0 {
            tracing::info!("{what}: accepting again after {} failed accepts", self.failures);
            self.failures = 0;
        }
    }
}

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

/// Bidirectional proxy with 64 KiB buffers and a 5-minute idle timeout.
///
/// Copies `a → b` and `b → a` concurrently, with buffers 8x larger than
/// tokio's default to cut read syscalls for typical HTTP payloads.
///
/// When one side stops sending (EOF), the write half of the other side is
/// shut down and the opposite direction keeps running, so a client that
/// half-closes after its request still receives the response. Returns once
/// both directions have finished, as soon as either fails, or when no data
/// has moved in *either* direction for [`IDLE_TIMEOUT`] — a long one-way
/// stream (a download, a log tail) is not idle.
pub async fn proxy_bidirectional<A, B>(a: A, b: B) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);
    let activity = Activity::new();

    let both = async {
        tokio::try_join!(
            copy_then_shutdown(&mut ar, &mut bw, &activity),
            copy_then_shutdown(&mut br, &mut aw, &activity),
        )
        .map(|_| ())
    };

    tokio::select! {
        result = both => result,
        () = activity.idle_for(IDLE_TIMEOUT) => Ok(()),
    }
}

/// Copy `reader` to `writer` until EOF, then shut down `writer` (a
/// half-close: the peer sees EOF but can keep sending the other way).
async fn copy_then_shutdown<R, W>(reader: &mut R, writer: &mut W, activity: &Activity) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        activity.touch();
        // Write piecewise so a slow but progressing reader counts as activity.
        let mut chunk = &buf[..n];
        while !chunk.is_empty() {
            let written = writer.write(chunk).await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            activity.touch();
            chunk = &chunk[written..];
        }
        writer.flush().await?;
    }
    writer.shutdown().await
}

/// When data last moved in either direction of a proxied connection.
struct Activity {
    start: tokio::time::Instant,
    /// Milliseconds after `start` of the latest activity.
    last_ms: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self { start: tokio::time::Instant::now(), last_ms: AtomicU64::new(0) }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(ms, Ordering::Relaxed);
    }

    /// Resolve once nothing has happened for `limit`.
    async fn idle_for(&self, limit: Duration) {
        loop {
            let last = self.start + Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
            let deadline = last + limit;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};
    use tokio::time::Instant;

    /// Start a proxy between a client and a backend; returns the outer ends
    /// (what the client and backend hold) and the proxy task.
    fn proxied() -> (DuplexStream, DuplexStream, tokio::task::JoinHandle<io::Result<()>>) {
        let (client, client_side) = duplex(1 << 16);
        let (backend_side, backend) = duplex(1 << 16);
        let proxy = tokio::spawn(proxy_bidirectional(client_side, backend_side));
        (client, backend, proxy)
    }

    #[tokio::test(start_paused = true)]
    async fn one_way_stream_outlives_the_idle_timeout() {
        let (mut client, mut backend, proxy) = proxied();
        let start = Instant::now();
        // The backend streams for twice the idle timeout; the client only reads.
        let ticks = (2 * IDLE_TIMEOUT.as_secs() / 10) as usize;
        tokio::spawn(async move {
            for _ in 0..ticks {
                backend.write_all(b"tick").await.unwrap();
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            backend.shutdown().await.unwrap();
        });
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        assert_eq!(received.len(), ticks * 4, "every tick arrived");
        assert!(start.elapsed() >= 2 * IDLE_TIMEOUT - Duration::from_secs(10));
        client.shutdown().await.unwrap();
        proxy.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_tunnel_closes_after_the_idle_timeout() {
        let (_client, _backend, proxy) = proxied();
        let start = Instant::now();
        proxy.await.unwrap().unwrap();
        assert_eq!(start.elapsed().as_secs(), IDLE_TIMEOUT.as_secs());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timer_restarts_on_traffic() {
        let (mut client, mut backend, proxy) = proxied();
        let start = Instant::now();
        tokio::time::sleep(IDLE_TIMEOUT - Duration::from_secs(1)).await;
        client.write_all(b"x").await.unwrap();
        let mut byte = [0u8; 1];
        backend.read_exact(&mut byte).await.unwrap();
        proxy.await.unwrap().unwrap();
        assert_eq!(start.elapsed().as_secs(), 2 * IDLE_TIMEOUT.as_secs() - 1);
    }

    #[tokio::test]
    async fn half_closed_client_still_gets_the_response() {
        let (mut client, mut backend, proxy) = proxied();
        // A backend that reads the whole request (to EOF) before replying.
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            backend.read_to_end(&mut request).await.unwrap();
            backend.write_all(b"response to ").await.unwrap();
            backend.write_all(&request).await.unwrap();
            backend.shutdown().await.unwrap();
        });
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response to request");
        server.await.unwrap();
        proxy.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_failed_direction_tears_down_both() {
        let (mut client, backend, proxy) = proxied();
        drop(backend);
        // Writing towards the vanished backend fails the proxy, which then
        // closes the client side too.
        client.write_all(b"data").await.unwrap();
        assert!(proxy.await.unwrap().is_err());
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
    }
}
