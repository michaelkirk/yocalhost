use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_staticfile::Static;
use hyper_util::rt::TokioIo;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};

/// An HTTP server for the files in a directory.
#[derive(Clone)]
pub(crate) struct StaticFiles {
    web_root: PathBuf,
    files: Static,
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

impl StaticFiles {
    pub fn http(web_root: PathBuf) -> Self {
        Self {
            files: Static::new(&web_root),
            web_root,
            stats: Arc::default(),
        }
    }

    pub fn web_root(&self) -> &Path {
        &self.web_root
    }

    pub async fn serve_connection(self, io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static) {
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
        if let Err(err) = http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .await
        {
            println!("Failed to serve connection: {:?}", err);
        }
    }
}
