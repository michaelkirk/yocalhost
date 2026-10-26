use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

mod link;
mod static_files;

use link::Link;
use static_files::StaticFiles;

#[macro_use]
extern crate log;

pub type Error = Box<dyn std::error::Error>;

/// Buffer between the throttled link and the built-in file server.
const STATIC_FILES_BUFFER_SIZE: usize = 64 * 1024;

/// An HTTP(S) file server, or a proxy for any TCP service, with artificial latency and bandwidth limits.
///
/// Throttling happens at the TCP layer, so TCP and TLS handshakes cost round trips like they
/// would on a real network.
///
/// You can start an instance from the command line:
/// ```ignore
/// # install yocalhost to your system
/// cargo install yocalhost
///
/// # start up a throttled server with 100ms latency and 1Mb bandwidth restrictions
/// yocalhost -p 8845 -l 100 -b "1Mb" -w path/to/web_root
///
/// # or put the same restrictions in front of a service on port 8080
/// yocalhost -p 8845 -l 100 -b "1Mb" --upstream 127.0.0.1:8080
/// ```
///
/// Or you invoke programatically, e.g. in your test suite setup.
/// ```no_run
/// use yocalhost::ThrottledServer;
/// use std::time::Duration;
/// # async fn start() {
/// let server = ThrottledServer::new(8845, Duration::from_millis(100), 1_000_000, "path/to/web_root");
/// server.spawn_in_background().await.expect("proper startup");
/// # }
///
/// // now you can make your requests against http://localhost:8845
/// ```
#[derive(Clone)]
pub struct ThrottledServer {
    port: u16,
    link: Link,
    upstream: Upstream,
}

#[derive(Clone)]
enum Upstream {
    Address(String),
    StaticFiles(StaticFiles),
}

impl ThrottledServer {
    /// Serves the files in `web_root` over HTTP.
    pub fn new(
        port: u16,
        latency: Duration,
        bytes_per_second: u64,
        web_root: impl Into<PathBuf>,
    ) -> Self {
        let files = StaticFiles::http(web_root.into());
        Self::with_upstream(
            port,
            latency,
            bytes_per_second,
            Upstream::StaticFiles(files),
        )
    }

    /// Serves the files in `web_root` over HTTPS, using a self-signed certificate.
    ///
    /// Clients must trust [`Self::certificate_pem`] or skip certificate verification.
    pub fn new_https(
        port: u16,
        latency: Duration,
        bytes_per_second: u64,
        web_root: impl Into<PathBuf>,
    ) -> Self {
        let files = StaticFiles::https(web_root.into());
        Self::with_upstream(
            port,
            latency,
            bytes_per_second,
            Upstream::StaticFiles(files),
        )
    }

    /// Forwards each connection to the service at `upstream`, e.g. "127.0.0.1:8080".
    pub fn new_proxy(
        port: u16,
        latency: Duration,
        bytes_per_second: u64,
        upstream: impl Into<String>,
    ) -> Self {
        let upstream = Upstream::Address(upstream.into());
        Self::with_upstream(port, latency, bytes_per_second, upstream)
    }

    fn with_upstream(
        port: u16,
        latency: Duration,
        bytes_per_second: u64,
        upstream: Upstream,
    ) -> Self {
        Self {
            port,
            link: Link::new(latency, bytes_per_second),
            upstream,
        }
    }

    #[cfg(test)]
    fn test(latency: Duration, bandwidth: byte_unit::Byte) -> Self {
        Self::new(Self::next_port(), latency, bandwidth.as_u64(), "test_data")
    }

    /// The directory being served, unless this is a proxy.
    pub fn web_root(&self) -> Option<&Path> {
        match &self.upstream {
            Upstream::Address(_) => None,
            Upstream::StaticFiles(files) => Some(files.web_root()),
        }
    }

    /// The self-signed certificate of a server created with [`Self::new_https`].
    pub fn certificate_pem(&self) -> Option<&str> {
        match &self.upstream {
            Upstream::Address(_) => None,
            Upstream::StaticFiles(files) => files.certificate_pem(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Starts accepting connections on a background task.
    pub async fn spawn_in_background(&self) -> Result<(), Error> {
        let listener = self.bind().await?;
        tokio::spawn(self.clone().accept_connections(listener));
        Ok(())
    }

    pub async fn serve(self) {
        let listener = self
            .bind()
            .await
            .unwrap_or_else(|err| panic!("unable to bind to port {}: {err}", self.port));
        let addr = listener.local_addr().expect("bound listener");
        match &self.upstream {
            Upstream::Address(upstream) => {
                println!("Listening on {addr}, forwarding to {upstream}")
            }
            Upstream::StaticFiles(files) => {
                println!("Listening on {}://{addr}", files.scheme_name())
            }
        }
        self.accept_connections(listener).await
    }

    async fn bind(&self) -> std::io::Result<TcpListener> {
        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        TcpListener::bind(addr).await
    }

    async fn accept_connections(self, listener: TcpListener) {
        loop {
            let (client, _) = listener
                .accept()
                .await
                .expect("listener to bind to accept.");
            let _ = client.set_nodelay(true);
            tokio::spawn(self.clone().handle_connection(client));
        }
    }

    async fn handle_connection(self, client: TcpStream) {
        match &self.upstream {
            Upstream::Address(addr) => match TcpStream::connect(addr).await {
                Ok(upstream) => {
                    let _ = upstream.set_nodelay(true);
                    self.link.relay(client, upstream).await
                }
                Err(err) => warn!("unable to connect to upstream {addr}: {err}"),
            },
            Upstream::StaticFiles(files) => {
                let (link_end, server_end) = tokio::io::duplex(STATIC_FILES_BUFFER_SIZE);
                tokio::spawn(files.clone().serve_connection(server_end));
                self.link.relay(client, link_end).await
            }
        }
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

impl Debug for ThrottledServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThrottledServer")
            .field("port", &self.port)
            .field("link", &self.link)
            .field("web_root", &self.web_root())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use byte_unit::Byte;
    use http_body_util::{BodyExt, Empty};
    use hyper::body::{Bytes, Incoming};
    use hyper::{Request, Response};
    use hyper_util::client::legacy::{connect::HttpConnector, Client};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio_rustls::rustls;

    type GetClient = Client<HttpConnector, Empty<&'static [u8]>>;

    fn build_http_get_client() -> GetClient {
        Client::builder(TokioExecutor::new()).build_http()
    }

    #[test]
    fn next_port_increments() {
        let first = ThrottledServer::next_port();
        let second = ThrottledServer::next_port();
        let third = ThrottledServer::next_port();
        assert!(first < second);
        assert!(second < third);
    }

    async fn make_1mb_request(client: GetClient, port: u16) -> Response<Incoming> {
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
        let latency = server.link.latency();
        assert!(elapsed > 2 * latency);
        assert!(elapsed < 3 * latency);
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
        let latency = server.link.latency();
        assert!(elapsed > 2 * latency);
        assert!(elapsed < 3 * latency);
    }

    #[tokio::test]
    async fn new_connection_costs_a_handshake_round_trip() {
        for latency in [Duration::from_millis(50), Duration::from_millis(100)] {
            let server = ThrottledServer::test(latency, Byte::from_str("1 Gb").unwrap());
            server.spawn_in_background().await.unwrap();
            let tolerance = Duration::from_millis(15);

            let started = Instant::now();
            let tcp = TcpStream::connect(("127.0.0.1", server.port))
                .await
                .unwrap();
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
                .await
                .unwrap();
            tokio::spawn(connection);
            get_gatsby(&mut sender).await;
            assert_near!(started.elapsed(), 2 * latency, tolerance);

            let started = Instant::now();
            get_gatsby(&mut sender).await;
            assert_near!(started.elapsed(), latency, tolerance);
        }
    }

    async fn get_gatsby(sender: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>>) {
        let request = Request::get("/gatsby.txt")
            .header("Host", "localhost")
            .body(Empty::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        response.collect().await.unwrap();
    }

    /// Connects to `server` over TLS, offering `alpn_protocols`.
    async fn tls_connect(
        server: &ThrottledServer,
        alpn_protocols: &[&[u8]],
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let certificate = rustls::pki_types::pem::PemObject::from_pem_slice(
            server.certificate_pem().unwrap().as_bytes(),
        )
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = alpn_protocols.iter().map(|p| p.to_vec()).collect();

        let tcp = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap()
    }

    fn https_test_server(latency: Duration) -> ThrottledServer {
        ThrottledServer::new_https(
            ThrottledServer::next_port(),
            latency,
            Byte::from_str("1 Gb").unwrap().as_u64(),
            "test_data",
        )
    }

    fn gatsby_request() -> Request<Empty<Bytes>> {
        Request::get("https://localhost/gatsby.txt")
            .header("Host", "localhost")
            .body(Empty::new())
            .unwrap()
    }

    #[tokio::test]
    async fn https() {
        let latency = Duration::from_millis(50);
        let server = https_test_server(latency);
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let tls = tls_connect(&server, &[b"http/1.1"]).await;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(connection);
        let response = sender.send_request(gatsby_request()).await.unwrap();
        let bytes = response.collect().await.unwrap().to_bytes();
        assert!(String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("West Egg"));

        // TCP handshake, TLS 1.3 handshake, then the request.
        assert_near!(now.elapsed(), 3 * latency, Duration::from_millis(20));
    }

    #[tokio::test]
    async fn http2_multiplexes_requests() {
        let latency = Duration::from_millis(50);
        let server = https_test_server(latency);
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let tls = tls_connect(&server, &[b"h2"]).await;
        let (sender, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
                .await
                .unwrap();
        tokio::spawn(connection);
        let get = |mut sender: hyper::client::conn::http2::SendRequest<Empty<Bytes>>| async move {
            let response = sender.send_request(gatsby_request()).await.unwrap();
            assert_eq!(response.version(), hyper::Version::HTTP_2);
            response.collect().await.unwrap();
        };
        tokio::join!(get(sender.clone()), get(sender));

        // Both requests share one round trip after the TCP and TLS handshakes.
        assert_near!(now.elapsed(), 3 * latency, Duration::from_millis(20));
    }

    #[tokio::test]
    async fn proxy() {
        let unthrottled = ThrottledServer::test(Duration::ZERO, Byte::from_str("1 GB").unwrap());
        unthrottled.spawn_in_background().await.unwrap();

        let latency = Duration::from_millis(50);
        let proxy = ThrottledServer::new_proxy(
            ThrottledServer::next_port(),
            latency,
            Byte::from_str("1 GB").unwrap().as_u64(),
            format!("127.0.0.1:{}", unthrottled.port),
        );
        proxy.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let response = make_1mb_request(build_http_get_client(), proxy.port).await;
        let bytes = response.collect().await.unwrap().to_bytes();
        assert_eq!(bytes.len(), 1_000_000);
        assert_near!(now.elapsed(), 2 * latency, Duration::from_millis(20));
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
