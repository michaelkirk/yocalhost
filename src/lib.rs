use async_speed_limit::Limiter;
#[cfg(test)]
use byte_unit::Byte;
use futures::StreamExt;
use hyper::{
    body::Bytes,
    http::uri::{Authority, Scheme},
    service::{make_service_fn, service_fn},
    Body, Client, Request, Response, Server as HyperServer, Uri,
};
use hyper_staticfile::Static;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

use std::convert::Infallible;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[macro_use]
extern crate log;

pub type Error = Box<dyn std::error::Error>;

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

async fn handle(
    req: Request<Body>,
    latency: Duration,
    limiter: Limiter,
    static_files: Static,
    stats: Stats,
) -> Result<Response<Body>, hyper::Error> {
    // simulate latency
    // REVIEW: should we use Delay?
    sleep(latency).await;
    throttled_download(req, limiter, static_files, stats).await
    // throttled_proxy(req, bandwidth).await
}

async fn throttled_response(
    mut response: Response<Body>,
    limiter: Limiter,
    stats: Stats,
) -> Result<Response<Body>, hyper::Error> {
    let mut response_body = Body::empty();
    std::mem::swap(&mut response_body, response.body_mut());

    let (mut sender, body) = hyper::body::Body::channel();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut response = stats::Response::default();
        while let Some(chunk) = response_body.next().await {
            let buffer_1 = chunk.unwrap().to_vec();
            response.len += buffer_1.len();
            let mut limited_buffer = limiter.clone().limit(Cursor::new(&buffer_1));
            let mut buffer_2 = vec![];

            // reading limited buffer to end
            limited_buffer.read_to_end(&mut buffer_2).await.unwrap();

            let back_out_again = Bytes::from(buffer_2);
            // Theres an error occuring during tests here, it doesn't seem to interfere
            // with the tests. Maybe we need a more judicious shutdown to avoid the spurious
            // output?
            match sender.send_data(back_out_again).await {
                Ok(()) => (),
                // This case often happens when client disconnects after a successful request.
                // It'd be nice to ensure all data had been sent, but I'm not sure.
                Err(hyper_error) if hyper_error.is_closed() => break,
                Err(other) => panic!("err: {other}"),
            }
        }
        let mut stats = stats.lock().unwrap();
        if cfg!(feature = "stats") {
            stats.push_response(response);
            stats.print_summary()
        }
    });

    *response.body_mut() = body;
    Ok(response)
}

async fn throttled_download(
    req: Request<Body>,
    limiter: Limiter,
    static_files: Static,
    stats: Stats,
) -> Result<Response<Body>, hyper::Error> {
    let response = static_files
        .serve(req)
        .await
        .expect("TODO: handle error conversion");
    throttled_response(response, limiter, stats).await
}

#[allow(unused)]
async fn throttled_proxy(
    mut req: Request<Body>,
    limiter: Limiter,
    stats: Stats,
) -> Result<Response<Body>, hyper::Error> {
    let uri = req.uri().clone();
    let mut parts = uri.into_parts();
    let authority = Authority::from_str("localhost:8000").expect("valid host");
    parts.authority = Some(authority);
    parts.scheme = Some(Scheme::HTTP);

    let uri = Uri::from_parts(parts).expect("valid uri parts");
    *req.uri_mut() = uri;
    let client = Client::new();
    let response = client.request(req).await?;
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
    bytes_per_second: usize,
    web_root: PathBuf,
}

impl ThrottledServer {
    #[cfg(test)]
    fn test(latency: Duration, bandwidth: Byte) -> Self {
        let web_root = "test_data";
        Self::new(
            Self::next_port(),
            latency,
            bandwidth.get_bytes() as usize,
            &PathBuf::from(web_root),
        )
    }

    pub fn new(
        port: u16,
        latency: Duration,
        bytes_per_second: usize,
        web_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            port,
            latency,
            bytes_per_second,
            web_root: web_root.into(),
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
        tokio::spawn(server.serve());
        self.wait_for_start().await
    }

    pub async fn serve(self) {
        let latency = self.latency;
        let web_root = self.web_root.clone();

        let bytes_per_second = self.bytes_per_second;
        // simulate bandwidth
        //
        // Limiter is created at the outer level and cloned in so that all connections have a
        // single shared limit, otherwise clients could get around the limit by making concurrent
        // requests.
        //
        // If you need to enforce separate limits, start a new server instance on a new port.
        let limiter = <Limiter>::new(bytes_per_second as f64);
        let stats = Arc::new(Mutex::new(stats::Stats::default()));

        let make_service = make_service_fn(move |_socket| {
            let limiter = limiter.clone();
            let static_files = Static::new(web_root.clone());
            let stats = stats.clone();
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    handle(
                        req,
                        latency,
                        limiter.clone(),
                        static_files.clone(),
                        stats.clone(),
                    )
                }))
            }
        });

        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let server = HyperServer::bind(&addr).serve(make_service);

        println!("Listening on {addr:?}...");
        if let Err(e) = server.await {
            error!("error: {}", e);
        }
    }

    async fn wait_for_start(&self) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel();
        let port = self.port;
        tokio::spawn(async move {
            async fn ping_server(port: u16) -> bool {
                let request = Request::builder()
                    .uri(format!("http://localhost:{port}/1M.txt"))
                    .body(Body::empty())
                    .expect("valid request");

                let client = Client::new();
                client
                    .request(request)
                    .await
                    .map(|resp| resp.status().is_success())
                    .unwrap_or(false)
            }

            loop {
                if ping_server(port).await {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn next_port_increments() {
        let first = ThrottledServer::next_port();
        let second = ThrottledServer::next_port();
        let third = ThrottledServer::next_port();
        assert!(first < second);
        assert!(second < third);
    }

    async fn make_1mb_request(port: u16) -> Response<Body> {
        let client = Client::new();
        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .body(Body::empty())
            .expect("valid request");
        client.request(request).await.unwrap()
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
            let range = ($actual - $tolerance..$actual + $tolerance);
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
        let now = Instant::now();
        make_1mb_request(server.port).await;
        let elapsed = now.elapsed();
        assert!(elapsed > server.latency);
        assert!(elapsed < 2 * server.latency);
    }

    #[tokio::test]
    async fn port_number() {
        let server =
            ThrottledServer::test(Duration::from_millis(50), Byte::from_str("1 Mb").unwrap());
        server.spawn_in_background().await.unwrap();
        let now = Instant::now();
        make_1mb_request(server.port).await;
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

            let now = Instant::now();
            make_1mb_request(server.port).await;
            let elapsed = now.elapsed();
            assert_near!(elapsed, latency, Duration::from_millis(10));
        }
        {
            let latency = Duration::from_millis(100);
            let server = ThrottledServer::test(latency, Byte::from_str("1 Gb").unwrap());
            server.spawn_in_background().await.unwrap();

            let now = Instant::now();
            make_1mb_request(server.port).await;
            let elapsed = now.elapsed();
            assert_near!(elapsed, latency, Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn high_bandwidth_should_complete_quickly() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_str("100 Mb").unwrap());
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let response = make_1mb_request(server.port).await;
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 1_000_000);

        let elapsed = now.elapsed();
        assert_near!(
            elapsed,
            Duration::from_millis(11),
            Duration::from_millis(10)
        );
    }

    #[tokio::test]
    async fn low_bandwidth_should_complete_slowly() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_bytes(500_000));
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let response = make_1mb_request(server.port).await;
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 1_000_000);

        let elapsed = now.elapsed();

        let expected = Duration::from_millis(2000);
        let tolerance = Duration::from_millis(500);
        assert_near!(elapsed, expected, tolerance);
    }

    #[tokio::test]
    async fn concurrent_requests_should_share_bandwidth() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_bytes(500_000));
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let (response_1, response_2) =
            tokio::join!(make_1mb_request(server.port), make_1mb_request(server.port));

        let bytes_1 = hyper::body::to_bytes(response_1.into_body()).await.unwrap();
        assert_eq!(bytes_1.len(), 1_000_000);
        let bytes_2 = hyper::body::to_bytes(response_2.into_body()).await.unwrap();
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
        let client = Client::new();
        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=0-8")
            .body(Body::empty())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 9);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "A Project");

        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=8754-8772")
            .body(Body::empty())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 19);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "I lived at West Egg");

        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .header("Range", "bytes=8765-8772")
            .body(Body::empty())
            .expect("valid request");
        let response = client.request(request).await.unwrap();
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 8);
        let string = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(string, "West Egg");
    }
}
