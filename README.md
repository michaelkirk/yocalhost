## localhost too fast? try yocalhost.

yocalhost is a throttled HTTP(S) development server: it serves a directory of
files with artificial latency and bandwidth limits. It supports Range requests,
and HTTPS (HTTP/1.1 or HTTP/2) with a self-signed certificate.

Throttling happens at the TCP layer, not per HTTP request: every packet is
delayed by half the round trip latency, and bandwidth is shared across all
concurrent connections. So opening a connection costs a TCP handshake round
trip, and HTTPS costs its TLS handshake too, like on a real network.

Because it works below HTTP, yocalhost can also stand in front of any TCP
service - your own HTTP API, a database, anything listening on a port - and
throttle the connections to it.

### Motivation

Evaluating against a naive localhost server, where latency and bandwidth are
unrealistitcally fast, does little to tell you how your network access will
perform in the real world.

Evaluating a network client against a remote host is subject to the whim of the
rest of the internet. Especially if your client involves many requests, it can
be hard to get consistent measurements.

With yocalhost, you run a local http server, but specify artificial bandwidth
and latency limitations. This allows you to measure realistic(ish) implications
of latency and bandwidth on your code in a reproducible way.

### Installing the yocalhost development server CLI

Assuming you have [rust and cargo installed](https://rustup.rs), you can install yocalhost with:

`cargo install yocahost`

Then start with:
```
cd your_web_root

yocalhost

# Start on port 8888, simulating a 100 megabit connection with 50ms latency
yocalhost -p 8888 -b "100Mbit" -l 50

# Serve over HTTPS with a self-signed certificate (e.g. `curl -k`)
yocalhost --https

# Throttle a service you're already running on port 8080
yocalhost --upstream 127.0.0.1:8080

# See other options
yocalhost --help
```

### Using yocalhost's `ThrottledServer` in your automated rust benchmarks

```
// e.g. in a criterion benchmark

use yocalhost::ThrottledServer;

let port = 8888;
let latency_ms = 50;
let bytes_per_second = 100_000_000 / 8;
let web_root = "../../test/data";

let server = ThrottledServer::new(port, latency_ms, bytes_per_second, web_root);
// or ThrottledServer::new_https(...), trusting `server.certificate_pem()` in your client
// or ThrottledServer::new_proxy(port, latency_ms, bytes_per_second, "127.0.0.1:8080")
    
let runtime = tokio::runtime::Runtime::new().unwrap(); : Runtime    
runtime.spawn(async move {
    server.serve().await;
});

c.bench_function("my network test", |b| {
    b.to_async(runtime).iter(|| {
        do_some_work(format!("http://localhost:{port}")
    })
})

```
