use async_speed_limit::Limiter;
use hyper::body::Bytes;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant};

const READ_BUFFER_SIZE: usize = 8 * 1024;

/// A simulated network link, shared by every connection across it.
#[derive(Clone, Debug)]
pub(crate) struct Link {
    latency: Duration,
    download: Limiter,
}

impl Link {
    pub fn new(latency: Duration, bytes_per_second: u64) -> Self {
        Self {
            latency,
            download: Limiter::new(bytes_per_second as f64),
        }
    }

    #[cfg(test)]
    pub fn latency(&self) -> Duration {
        self.latency
    }

    /// Relays bytes between `client` and `upstream` as if they were `latency` (round trip) apart.
    pub async fn relay(
        &self,
        client: TcpStream,
        upstream: impl AsyncRead + AsyncWrite + Send + 'static,
    ) {
        // Like TCP, nothing flows until the SYN/SYN-ACK round trip completes.
        let handshake_done = Instant::now() + self.latency;
        let (client_read, client_write) = client.into_split();
        let (upstream_read, upstream_write) = tokio::io::split(upstream);
        let (bytes_up, bytes_down) = tokio::join!(
            self.forward(client_read, upstream_write, handshake_done),
            self.forward(
                self.download.clone().limit(upstream_read),
                client_write,
                handshake_done
            ),
        );
        if cfg!(feature = "stats") {
            debug!("connection closed. bytes_up: {bytes_up}, bytes_down: {bytes_down}");
        }
    }

    /// Delivers everything read from `from` to `to` half a round trip later, returning the byte count.
    async fn forward(
        &self,
        mut from: impl AsyncRead + Unpin,
        mut to: impl AsyncWrite + Unpin,
        handshake_done: Instant,
    ) -> usize {
        let one_way = self.latency / 2;
        let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Bytes)>();
        let read = async move {
            let mut total = 0;
            let mut buffer = vec![0; READ_BUFFER_SIZE];
            while let Ok(len @ 1..) = from.read(&mut buffer).await {
                total += len;
                let deliver_at = Instant::now().max(handshake_done) + one_way;
                if tx
                    .send((deliver_at, Bytes::copy_from_slice(&buffer[..len])))
                    .is_err()
                {
                    break;
                }
            }
            total
        };
        let write = async move {
            while let Some((deliver_at, bytes)) = rx.recv().await {
                sleep_until(deliver_at).await;
                if to.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            let _ = to.shutdown().await;
        };
        tokio::join!(read, write).0
    }
}
