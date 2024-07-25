## localhost too fast? try yocalhost.

yocalhost is an http development server that simulates latency and bandwidth limitations. 

It supports Range requests, and will throttle bandwidth across all concurrent connections.

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
