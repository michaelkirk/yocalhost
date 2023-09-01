## localhost too fast? try yocalhost.

yocalhost is an http development server that simulates latency and bandwidth limitations.

Evaluating a network client against a remote host is subject to the whim of the
rest of the internet. Especially if your client involves many requests, it can
be hard to get consistent measurements.

Evaluating against a naive localhost server, where latency and bandwidth are
unreasonably fast, can draw a false picture.

With yocalhost, you run a local http server, but specify artificial bandwidth
and latency limitations. This allows you to measure realistic(ish) implications
of latency and bandwidth on your code in a reproducible way.
