use hyper::service::service_fn;
use hyper_staticfile::Static;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

/// An HTTP(S) server for the files in a directory.
#[derive(Clone)]
pub(crate) struct StaticFiles {
    web_root: PathBuf,
    files: Static,
    scheme: Scheme,
    stats: Arc<Mutex<Stats>>,
}

#[derive(Debug, Default)]
struct Stats {
    req_count: usize,
    body_bytes_sent: u64,
}

impl Stats {
    fn record<B>(&mut self, response: &hyper::Response<B>) {
        self.req_count += 1;
        self.body_bytes_sent += response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|len| len.to_str().ok()?.parse::<u64>().ok())
            .unwrap_or(0);
        debug!(
            "req_count: {}, body_bytes_sent: {}",
            self.req_count, self.body_bytes_sent
        );
    }
}

#[derive(Clone)]
enum Scheme {
    Http,
    Https(Tls),
}

#[derive(Clone)]
struct Tls {
    acceptor: TlsAcceptor,
    certificate_pem: String,
}

impl Tls {
    fn self_signed() -> Self {
        let certified_key = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("valid certificate params");
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            certified_key.signing_key.serialize_der().into(),
        );
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("supported protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![certified_key.cert.der().clone()], key)
        .expect("valid certificate");
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            certificate_pem: certified_key.cert.pem(),
        }
    }
}

impl StaticFiles {
    pub fn http(web_root: PathBuf) -> Self {
        Self::new(web_root, Scheme::Http)
    }

    /// Serves over HTTPS with a freshly generated self-signed certificate.
    pub fn https(web_root: PathBuf) -> Self {
        Self::new(web_root, Scheme::Https(Tls::self_signed()))
    }

    fn new(web_root: PathBuf, scheme: Scheme) -> Self {
        Self {
            files: Static::new(&web_root),
            web_root,
            scheme,
            stats: Arc::default(),
        }
    }

    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    pub fn scheme_name(&self) -> &'static str {
        match self.scheme {
            Scheme::Http => "http",
            Scheme::Https(_) => "https",
        }
    }

    pub fn certificate_pem(&self) -> Option<&str> {
        match &self.scheme {
            Scheme::Http => None,
            Scheme::Https(tls) => Some(&tls.certificate_pem),
        }
    }

    pub async fn serve_connection(self, io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static) {
        let result = match &self.scheme {
            Scheme::Http => self.serve_http(io).await,
            Scheme::Https(tls) => match tls.acceptor.accept(io).await {
                Ok(io) => self.serve_http(io).await,
                Err(err) => {
                    debug!("TLS handshake failed: {err}");
                    return;
                }
            },
        };
        if let Err(err) = result {
            println!("Failed to serve connection: {:?}", err);
        }
    }

    async fn serve_http(
        &self,
        io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let files = self.files.clone();
        let stats = self.stats.clone();
        let service = service_fn(move |request| {
            let (files, stats) = (files.clone(), stats.clone());
            async move {
                let response = files.serve(request).await?;
                if cfg!(feature = "stats") {
                    stats.lock().unwrap().record(&response);
                }
                Ok::<_, std::io::Error>(response)
            }
        });
        auto::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(io), service)
            .await
    }
}
