use std::fmt::Debug;
use async_speed_limit::Limiter;
#[cfg(test)]
use byte_unit::Byte;
use futures::StreamExt;
use hyper::{
    body::Bytes,
    Request, Response, Uri,
};
use hyper_staticfile::Static;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

use http_body_util::{Empty, StreamBody};
use hyper::body::{Body, Buf, Frame, Incoming as IncomingBody};
use hyper::server::conn::http1;
use hyper::service::Service;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::future::Future;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

#[macro_use]
extern crate log;

pub type Error = Box<dyn std::error::Error>;

// It seems crazy that we need to be this verbose for a basic "HTTP Get client",
// but I'm not sure how to make it simpler.
type GetClient = Client<HttpConnector, Empty<&'static [u8]>>;

fn build_http_get_client() -> GetClient {
    Client::builder(TokioExecutor::new()).build_http()
}

mod stats {
    #[derive(Debug, Default)]
    pub(crate) struct Response {
        pub len: usize,
    }

    #[derive(Debug, Default)]
    pub(crate) struct Stats {
        pub responses: Vec<Response>,
    }

    impl Stats {
        pub fn print_summary(&self) {
            let req_count = self.responses.len();
            let body_bytes_sent: usize = self.responses.iter().map(|r| r.len).sum();
            debug!("req_count: {req_count}, body_bytes_sent: {body_bytes_sent}");
        }

        pub fn push_response(&mut self, response: Response) {
            self.responses.push(response)
        }
    }
}
type Stats = Arc<Mutex<stats::Stats>>;

async fn throttled_response<B: Body + Send + Unpin + 'static>(
    upstream_response: Response<B>,
    limiter: Limiter,
    stats: Stats,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error>
where
    B::Error: Debug,
    B::Data: Debug + Send,
{
    let (upstream_parts, upstream_body) = upstream_response.into_parts();

    // REVIEW: buffer size
    let buffer_size = 8 * 1024;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, hyper::Error>>(buffer_size);

    let byte_stream = ReceiverStream::new(rx);

    tokio::spawn(async move {
        let mut response_stats = stats::Response::default();

        let mut data_stream = upstream_body.collect().await.unwrap().into_data_stream();
        while let Some(chunk) = data_stream.next().await.transpose().unwrap() {
            let buffer_1 = chunk.chunk();
            response_stats.len += buffer_1.len();
            let mut limited_buffer = limiter.clone().limit(Cursor::new(&buffer_1));
            let mut buffer_2 = vec![];

            limited_buffer.read_to_end(&mut buffer_2).await.unwrap();

            let back_out_again = Bytes::from(buffer_2);
            // There's an error occurring during tests here, it doesn't seem to interfere
            // with the tests. Maybe we need a more judicious shutdown to avoid the spurious
            // output?
            match tx.send(Ok(Frame::data(back_out_again))).await {
                Ok(()) => (),
                // This case often happens when client disconnects after a successful request.
                // It'd be nice to ensure all data had been sent, but I'm not sure.
                // Err(hyper_error) if hyper_error.is_closed() => break,
                Err(other) => panic!("err: {other}"),
            }
        }
        let mut stats = stats.lock().unwrap();
        if cfg!(feature = "stats") {
            stats.push_response(response_stats);
            stats.print_summary()
        }
    });

    let stream_body = StreamBody::new(byte_stream);
    Ok(Response::from_parts(
        upstream_parts,
        BoxBody::new(stream_body),
    ))
}

async fn throttled_download(
    req: Request<hyper::body::Incoming>,
    limiter: Limiter,
    static_files: Static,
    stats: Stats,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let response = static_files
        .serve(req)
        .await
        .expect("TODO: handle error conversion");
    throttled_response(response, limiter, stats).await
}

/// An HTTP server that imposes artificial latency and bandwidth limits.
///
/// ```no_run
/// use yocalhost::ThrottledServer;
/// use std::time::Duration;
/// # async fn start() {
/// let server = ThrottledServer::new(8845, Duration::from_millis(100), 1_000_000, "path/to/test-fixtures");
/// server.spawn_in_background().await.expect("proper startup");
/// # }
///
/// // now you can make your requests against http://localhost:9999
/// ```
#[derive(Clone, Debug)]
pub struct ThrottledServer {
    port: u16,
    latency: Duration,
    limiter: Limiter,
    stats: Stats,
    web_root: PathBuf,
    client: GetClient,
}

impl ThrottledServer {
    #[cfg(test)]
    fn test(latency: Duration, bandwidth: Byte) -> Self {
        let web_root = "test_data";
        Self::new(
            Self::next_port(),
            latency,
            bandwidth.as_u64(),
            PathBuf::from(web_root),
        )
    }

    pub fn new(
        port: u16,
        latency: Duration,
        bytes_per_second: u64,
        web_root: impl Into<PathBuf>,
    ) -> Self {
        let stats = Arc::new(Mutex::new(stats::Stats::default()));
        Self {
            port,
            latency,
            limiter: Limiter::new(bytes_per_second as f64),
            stats,
            web_root: web_root.into(),
            client: build_http_get_client(),
        }
    }

    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn spawn_in_background(&self) -> Result<(), Error> {
        let server = self.clone();
        tokio::spawn(server.clone().serve());
        self.wait_for_start().await
    }

    async fn wait_for_start(&self) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        let port = self.port;
        let client = self.client.clone();

        tokio::spawn(async move {
            let uri = format!("http://localhost:{port}/1M.txt")
                .parse::<Uri>()
                .unwrap();

            loop {
                let is_up = client
                    .get(uri.clone())
                    .await
                    .map(|resp| resp.status().is_success())
                    .unwrap_or(false);

                if is_up {
                    debug!("ping succeeded");
                    tx.send(()).unwrap();
                    break;
                } else {
                    debug!("ping failed");
                }
                sleep(Duration::from_micros(100)).await;
            }
        });

        let _ = timeout(self.latency + Duration::from_millis(400), rx)
            .await
            .expect("startup timed out");

        debug!("finished starting");
        Ok(())
    }

    #[cfg(test)]
    fn next_port() -> u16 {
        static MUTEX: std::sync::Mutex<u16> = std::sync::Mutex::new(9901u16);
        let mut next = MUTEX.lock().unwrap();
        let value = *next;
        *next = value + 1;
        value
    }

    pub async fn serve(self) {
        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        let listener = TcpListener::bind(addr)
            .await
            .unwrap_or_else(|_| panic!("unable to bind to address: {addr:?}"));
        println!("Listening on http://{}", addr);

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .expect("listener to bind to accept.");

            let io = TokioIo::new(stream);
            let svc_clone = self.clone();
            tokio::task::spawn(async move {
                if let Err(err) = http1::Builder::new().serve_connection(io, svc_clone).await {
                    println!("Failed to serve connection: {:?}", err);
                }
            });
        }
    }
}

use http_body_util::{combinators::BoxBody, BodyExt};
use hyper_util::client::legacy::connect::HttpConnector;
use tokio::io::AsyncReadExt;
use tokio_stream::wrappers::ReceiverStream;

impl Service<Request<IncomingBody>> for ThrottledServer {
    type Response = Response<BoxBody<Bytes, hyper::Error>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<IncomingBody>) -> Self::Future {
        let static_files = Static::new(self.web_root.clone());
        let limiter = self.limiter.clone();
        let latency = self.latency;
        let stats = self.stats.clone();

        Box::pin(async move {
            sleep(latency).await;
            throttled_download(req, limiter, static_files, stats).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::time::Instant;
    use std::str::FromStr;

    #[test]
    fn next_port_increments() {
        let first = ThrottledServer::next_port();
        let second = ThrottledServer::next_port();
        let third = ThrottledServer::next_port();
        assert!(first < second);
        assert!(second < third);
    }

    async fn make_1mb_request(client: GetClient, port: u16) -> Response<impl Body> {
        client
            .get(format!("http://localhost:{port}/1M.txt").parse().unwrap())
            .await
            .expect("valid request")
    }

    /// ```
    /// let actual = Duration::from_millis(97);
    /// let expected = Duration::from_millis(100);
    /// let tolerance = Duration::from_millis(4);
    ///
    /// assert_near!(actual, expected, tolerance);
    /// ```
    macro_rules! assert_near {
        ($actual:expr, $expected:expr, $tolerance:expr) => {
            let range = ($actual.max($tolerance) - $tolerance..$actual + $tolerance);
            assert!(
                range.contains(&$expected),
                "{actual:?} not within {tolerance:?} of {expected:?}",
                actual = $actual,
                tolerance = $tolerance,
                expected = $expected
            );
        };
    }

    #[tokio::test]
    async fn starts() {
        let server =
            ThrottledServer::test(Duration::from_millis(50), Byte::from_str("1 Mb").unwrap());
        server.spawn_in_background().await.unwrap();
        let client = build_http_get_client();
        let now = Instant::now();
        make_1mb_request(client, server.port).await;
        let elapsed = now.elapsed();
        assert!(elapsed > server.latency);
        assert!(elapsed < 2 * server.latency);
    }

    #[tokio::test]
    async fn port_number() {
        let server =
            ThrottledServer::test(Duration::from_millis(50), Byte::from_str("1 Mb").unwrap());
        server.spawn_in_background().await.unwrap();
        let client = build_http_get_client();
        let now = Instant::now();
        make_1mb_request(client, server.port).await;
        let elapsed = now.elapsed();
        assert!(elapsed > server.latency);
        assert!(elapsed < 2 * server.latency);
    }

    #[tokio::test]
    async fn latency() {
        {
            let latency = Duration::from_millis(50);
            let server = ThrottledServer::test(latency, Byte::from_str("1 Gb").unwrap());
            server.spawn_in_background().await.unwrap();

            let client = build_http_get_client();
            let now = Instant::now();
            make_1mb_request(client, server.port).await;
            let elapsed = now.elapsed();
            assert_near!(elapsed, latency, Duration::from_millis(10));
        }
        {
            let latency = Duration::from_millis(100);
            let server = ThrottledServer::test(latency, Byte::from_str("1 Gb").unwrap());
            server.spawn_in_background().await.unwrap();

            let client = build_http_get_client();
            let now = Instant::now();
            make_1mb_request(client, server.port).await;
            let elapsed = now.elapsed();
            assert_near!(elapsed, latency, Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn high_bandwidth_should_complete_quickly() {
        let latency = Duration::from_millis(0);
        let server = ThrottledServer::test(latency, Byte::from_str("1 GB").unwrap());
        server.spawn_in_background().await.unwrap();

        let client = build_http_get_client();
        let now = Instant::now();
        let response = make_1mb_request(client, server.port).await;
        let bytes = response
            .collect()
            .await
            .unwrap_or_else(|_| panic!("response failed"))
            .to_bytes();
        assert_eq!(bytes.len(), 1_000_000);

        let elapsed = now.elapsed();
        // NOTE: This is known to fail in debug builds since upgrading to hyper 1.0
        // Apparently whatever hyper is doing now is much slower in debug builds
        // Or, more likely, I did something dumb when translating the new API.
        assert_near!(
            elapsed,
            Duration::from_millis(11),
            Duration::from_millis(10)
        );
    }

    #[tokio::test]
    async fn low_bandwidth_should_complete_slowly() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_u64(500_000));
        server.spawn_in_background().await.unwrap();

        let client = build_http_get_client();
        let now = Instant::now();
        let response = make_1mb_request(client, server.port).await;
        let bytes = response
            .collect()
            .await
            .unwrap_or_else(|_| panic!("response failed"))
            .to_bytes();
        assert_eq!(bytes.len(), 1_000_000);

        let elapsed = now.elapsed();

        let expected = Duration::from_millis(2000);
        let tolerance = Duration::from_millis(500);
        assert_near!(elapsed, expected, tolerance);
    }

    #[tokio::test]
    async fn concurrent_requests_should_share_bandwidth() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_u64(500_000));
        server.spawn_in_background().await.unwrap();

        let client = build_http_get_client();
        let now = Instant::now();
        let (response_1, response_2) = tokio::join!(
            make_1mb_request(client.clone(), server.port),
            make_1mb_request(client, server.port)
        );

        let bytes_1 = response_1
            .collect()
            .await
            .unwrap_or_else(|_| panic!("response failed"))
            .to_bytes();
        assert_eq!(bytes_1.len(), 1_000_000);
        let bytes_2 = response_2
            .collect()
            .await
            .unwrap_or_else(|_| panic!("response failed"))
            .to_bytes();
        assert_eq!(bytes_2.len(), 1_000_000);

        let elapsed = now.elapsed();
        let expected = Duration::from_millis(4000);
        let tolerance = Duration::from_millis(500);
        assert_near!(elapsed, expected, tolerance);
    }

    #[tokio::test]
    async fn range_request() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_str("10MB").unwrap());
        server.spawn_in_background().await.unwrap();

        let port = server.port;
        let client = build_http_get_client();
        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=0-8")
            .body(Empty::new())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = response.collect().await.unwrap().to_bytes();
        assert_eq!(bytes.len(), 9);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "A Project");

        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=8754-8772")
            .body(Empty::new())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = response.collect().await.unwrap().to_bytes();
        assert_eq!(bytes.len(), 19);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "I lived at West Egg");

        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=8765-8772")
            .body(Empty::new())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = response.collect().await.unwrap().to_bytes();
        assert_eq!(bytes.len(), 8);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "West Egg");
    }
}
