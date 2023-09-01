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
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

pub type Error = Box<dyn std::error::Error>;

async fn handle(
    req: Request<Body>,
    latency: Duration,
    bytes_per_second: usize,
    static_files: Static,
) -> Result<Response<Body>, hyper::Error> {
    // simulate latency
    // REVIEW: should we use Delay?
    sleep(latency).await;
    throttled_download(req, bytes_per_second, static_files).await
    // throttled_proxy(req, bandwidth).await
}

async fn throttled_response(
    mut response: Response<Body>,
    bytes_per_second: usize,
) -> Result<Response<Body>, hyper::Error> {
    let mut response_body = Body::empty();
    std::mem::swap(&mut response_body, response.body_mut());

    let (mut sender, body) = hyper::body::Body::channel();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        // simulate bandwidth
        let limiter = <Limiter>::new(bytes_per_second as f64);

        while let Some(chunk) = response_body.next().await {
            let buffer_1 = chunk.unwrap().to_vec();
            let mut limited_buffer = limiter.clone().limit(Cursor::new(&buffer_1));
            let mut buffer_2 = vec![];

            // reading limited buffer to end
            limited_buffer.read_to_end(&mut buffer_2).await.unwrap();

            let back_out_again = Bytes::from(buffer_2);
            // Theres an error occuring during tests here, it doesn't seem to interfere
            // with the tests. Maybe we need a more judicious shutdown to avoid the spurious
            // output?
            sender.send_data(back_out_again).await.unwrap();
        }
    });

    *response.body_mut() = body;
    Ok(response)
}

async fn throttled_download(
    req: Request<Body>,
    bytes_per_second: usize,
    static_files: Static,
) -> Result<Response<Body>, hyper::Error> {
    let response = static_files
        .serve(req)
        .await
        .expect("TODO: handle error conversion");
    throttled_response(response, bytes_per_second).await
}

#[allow(unused)]
async fn throttled_proxy(
    mut req: Request<Body>,
    bytes_per_second: usize,
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
    throttled_response(response, bytes_per_second).await
}

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

    pub async fn spawn_in_background(&self) -> Result<(), Error> {
        let server = self.clone();
        tokio::spawn(server.serve());
        self.wait_for_start().await
    }

    pub async fn serve(self) {
        let latency = self.latency;
        let bytes_per_second = self.bytes_per_second;
        let web_root = self.web_root.clone();

        let make_service = make_service_fn(move |_socket| {
            let static_files = Static::new(web_root.clone());
            async move {
                Ok::<_, Infallible>(service_fn(move |req| {
                    handle(req, latency, bytes_per_second, static_files.clone())
                }))
            }
        });

        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));
        let server = HyperServer::bind(&addr).serve(make_service);

        println!("Listening...");
        if let Err(e) = server.await {
            println!("error: {}", e);
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
                    println!("ping succeeded");
                    tx.send(()).unwrap();
                    break;
                } else {
                    println!("ping failed");
                }
                sleep(Duration::from_micros(100)).await;
            }
        });

        let _ = timeout(self.latency + Duration::from_millis(400), rx)
            .await
            .expect("startup timed out");

        println!("finished starting");
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
        assert_eq!(ThrottledServer::next_port(), 9901);
        assert_eq!(ThrottledServer::next_port(), 9902);
        assert_eq!(ThrottledServer::next_port(), 9903);
    }

    async fn make_1mb_request(port: u16) -> Response<Body> {
        let client = Client::new();
        let request = Request::builder()
            .uri(format!("http://localhost:{port}/1M.txt"))
            .body(Body::empty())
            .expect("valid request");
        client.request(request).await.unwrap()
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
            let server = ThrottledServer::test(latency, Byte::from_str("1 Mb").unwrap());
            server.spawn_in_background().await.unwrap();

            let now = Instant::now();
            make_1mb_request(server.port).await;
            let elapsed = now.elapsed();
            dbg!(elapsed);
            assert!(elapsed > latency, "request took less than {latency:?}");
            assert!(
                elapsed < 2 * latency,
                "request took longer than 2x{latency:?}"
            );
        }
        {
            let latency = Duration::from_millis(100);
            let server = ThrottledServer::test(latency, Byte::from_str("1 Mb").unwrap());
            server.spawn_in_background().await.unwrap();

            let now = Instant::now();
            make_1mb_request(server.port).await;
            let elapsed = now.elapsed();
            dbg!(elapsed);
            assert!(elapsed > latency, "request took less than {latency:?}");
            assert!(
                elapsed < 2 * latency,
                "request took longer than 2x{latency:?}"
            );
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
        assert!(elapsed < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn low_bandwidth_should_complete_slowly() {
        let latency = Duration::from_millis(1);
        let server = ThrottledServer::test(latency, Byte::from_str("0.75Mb").unwrap());
        server.spawn_in_background().await.unwrap();

        let now = Instant::now();
        let response = make_1mb_request(server.port).await;
        let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        assert_eq!(bytes.len(), 1_000_000);

        let elapsed = now.elapsed();
        assert!(elapsed > Duration::from_millis(1000));
        assert!(elapsed < Duration::from_millis(1500));
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
