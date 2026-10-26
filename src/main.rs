use byte_unit::Byte;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

use yocalhost::ThrottledServer;

#[derive(Debug, Parser)]
struct Args {
    /// Which port to serve on
    #[arg(short, long, default_value_t = 8001)]
    port: u16,

    /// round trip latency (in millis)
    #[arg(short, long, default_value_t = 100)]
    latency: u64,

    /// download bandwidth (per second) accepts a string like "1.1Mb" or "512kb"
    #[arg(short, long, default_value_t=Byte::from_u64(1000000))]
    bandwidth: Byte,

    /// Path to root directory of files hosted by the webserver.
    /// Defaults to present directory.
    #[arg(short, long, default_value = ".", conflicts_with = "upstream")]
    web_root: PathBuf,

    /// Serve files over HTTPS, with a self-signed certificate.
    #[arg(long, conflicts_with = "upstream")]
    https: bool,

    /// Instead of serving files, forward connections to this address, e.g. "127.0.0.1:8080".
    #[arg(short, long)]
    upstream: Option<String>,
}

#[tokio::main]
async fn main() {
    #[cfg(feature = "env_logger")]
    {
        env_logger::init();
    }
    let args = Args::parse();
    dbg!(&args);

    let port = args.port;
    let latency = Duration::from_millis(args.latency);
    let bytes_per_second = args.bandwidth.as_u64();

    let server = match args.upstream {
        Some(upstream) => ThrottledServer::new_proxy(port, latency, bytes_per_second, upstream),
        None if args.https => {
            ThrottledServer::new_https(port, latency, bytes_per_second, &args.web_root)
        }
        None => ThrottledServer::new(port, latency, bytes_per_second, &args.web_root),
    };

    server.serve().await
}
