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

    /// latency (in millis)
    #[arg(short, long, default_value_t = 100)]
    latency: u64,

    /// bandwidth (per second) accepts a string like "1.1Mb" or "512kb"
    #[arg(short, long, default_value_t=Byte::from_u64(1000000))]
    bandwidth: Byte,

    /// Path to root directory of files hosted by the webserver.
    /// Defaults to present directory.
    #[arg(short, long, default_value = ".")]
    web_root: PathBuf,
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
    let bandwidth = args.bandwidth;

    let server = ThrottledServer::new(port, latency, bandwidth.as_u64(), &args.web_root);

    // Not actually waiting here? Why isn't the server starting?
    server.serve().await
}
