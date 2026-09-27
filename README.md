# 67ft

Self-hosted paywall bypass proxy — a fast, single-binary Rust alternative to [12ft.io](https://12ft.io)

Pretends to be Googlebot to fetch full article content. Runs as a single static binary with no runtime dependencies. Designed to run on a Raspberry Pi 3B.

---

## Run (dev)

```sh
cargo run
# → http://localhost:8080
```

## Build (production)

```sh
cargo build --release
./target/release/67ft
```

## Options

```
67ft [OPTIONS]

  -p, --port <PORT>          Port to listen on  [env: PORT]  [default: 8080]
      --host <HOST>          Bind address       [env: HOST]  [default: 0.0.0.0]
  -u, --user-agent <UA>      Override User-Agent
      --x-forwarded-for <IP> Spoofed client IP  [default: 66.249.66.1]
      --timeout <SECS>       Request timeout    [default: 30]
      --max-body-mb <MB>     Max response size  [default: 25]
      --cache-ttl <SECS>     Page cache lifetime [default: 300, 0 disables]
      --cache-mb <MB>        Page cache budget   [default: 32]
      --max-concurrent <N>   In-flight requests  [default: 16]
      --allow-private-hosts  Permit loopback / private / link-local targets
```

## A note on exposure

The default bind is `0.0.0.0`, so the service is reachable from your whole
network. Targets that resolve to loopback, private, link-local or carrier-grade
NAT addresses are refused with `403`, and every redirect hop is re-checked, so
the proxy cannot be used to probe the network it runs on. Pass
`--allow-private-hosts` only when you trust everyone who can reach the port.

There is still no authentication. Anyone who can reach the port can make your
host fetch arbitrary public URLs under your IP address. Bind to `127.0.0.1`, or
put it behind something that authenticates, if that matters to you.

## URLs

| Endpoint | Description |
|---|---|
| `GET /` | Landing page with input box |
| `GET /{url}` | Proxy and render the page |
| `GET /raw/{url}` | Return raw proxied HTML |
| `GET /health` | Health check |

Example: `http://localhost:8080/https://example.com/some-article`

## Bookmarklet

Drag the "📖 67ft-ize" button on the landing page to your bookmarks bar.
Click it on any page to instantly proxy it through 67ft.

Or create it manually:
```js
javascript:(function(){window.location.href='http://localhost:8080/'+encodeURIComponent(window.location.href);})();
```

## How it works

- Sends requests with `User-Agent: Googlebot` and `X-Forwarded-For: 66.249.66.1`
- **Removes all `<script>` elements**, producing a static reader view. Publishers
  ship single-page apps that re-render from their own router and API state; left
  in place, that JavaScript discards the server-rendered article and shows its
  own 404 or paywall
- Unwraps `<noscript>` so lazy-loaded images become real images
- Injects `<base href="...">` so relative resources (CSS, images) load directly from origin
- Rewrites `<a href>` links to stay proxied through 67ft
- Rewrites `<form action>` so on-site search keeps working, and forwards the
  query string on to the target
- Decodes legacy charsets to UTF-8 rather than mangling them
- Strips CSP / X-Frame-Options headers that would block rendering
- Caches rendered pages in memory, and compresses responses on the way out

Because scripts are removed, interactive features such as comments, embedded
players and infinite scroll will not work. That is the trade for reliable text.

Note that major publishers verify crawlers by reverse DNS on the source IP, so a
forged user agent alone will not pass on every site.

## Run on a Raspberry Pi

One command, from this repo on your laptop:

```sh
./deploy/deploy.sh pi@raspberrypi.local
```

It asks the Pi what it is, cross-compiles a static musl binary to match,
installs it to `/usr/local/bin/67ft` with a hardened systemd unit, enables it
at boot and waits for `/health` to answer before claiming success. Re-run it to
upgrade; your `/etc/67ft.conf` is never overwritten.

If `sudo` on the Pi wants a password — a prompt no non-interactive script can
answer — it stages everything and hands you the one command to finish with.

Cross-compiling needs one of these on the laptop:

```sh
cargo install cargo-zigbuild && brew install zig   # lighter, no Docker
cargo install cross                                # needs Docker running
```

Or skip both and build on the Pi itself — minutes on a Pi 5 or a CM5, 20 to 40
on a 3B, where the script drops `lto` so the link fits in 1GB:

```sh
./deploy/deploy.sh pi@raspberrypi.local --native
```

### Alongside Pi-hole

They coexist, with two things to know.

**Ports.** Pi-hole v5 serves its admin page from lighttpd on 80; v6 serves it
from FTL on 80 and 443. 67ft defaults to 8080 and the installer refuses to
start if anything already holds that port, because moving the Pi-hole admin
page to 8080 is a common enough thing to have done.

**Priority.** The unit runs at `Nice=10` with half the normal CPU and IO
weight. A Pi 3B fetching and re-serializing a news page is enough to add
latency to every DNS lookup in the house, and the article can afford to wait
where the DNS cannot.

**Memory.** The `MemoryMax` in the unit is a backstop that, on a stock
Raspberry Pi, does nothing at all: the boards boot without the memory cgroup
controller, so systemd logs a warning and ignores it. Check with

```sh
grep memory /sys/fs/cgroup/cgroup.controllers   # no output means it is off
```

and turn it on, if you want it enforced, by adding `cgroup_enable=memory
cgroup_memory=1` to `/boot/firmware/cmdline.txt` and rebooting. What bounds the
process either way is `MAX_CONCURRENT` x `MAX_BODY_MB` plus the cache, set in
`/etc/67ft.conf` — which is why those are the numbers worth tuning.

### Configuration

Everything lives in `/etc/67ft.conf` as environment variables — the same
options as the command line, which `67ft --help` lists:

```sh
sudo nano /etc/67ft.conf
sudo systemctl restart 67ft
```

The shipped defaults assume a 4GB board — a Compute Module 5, a Pi 4 or 5 —
sharing with Pi-hole: 16 concurrent requests, 25MB maximum body, 128MB cache.
The worst case that matters is `MAX_CONCURRENT` whole bodies buffered at once,
so those two multiply to a 400MB ceiling. On a 1GB board such as a 3B or a
Zero 2, use 4 / 10 / 16 instead.

```sh
systemctl status 67ft          # is it up
journalctl -u 67ft -f          # what is it doing
```

The unit is `deploy/67ft.service` if you would rather install it by hand. Two
details in it are load-bearing and easy to get wrong:

- `RestrictAddressFamilies` **must** include `AF_NETLINK`. glibc's
  `getaddrinfo` opens a netlink socket to enumerate local interfaces before it
  will answer, so leaving it out makes every fetch fail name resolution while
  DNS on the Pi itself — Pi-hole included — looks perfectly healthy.
- `ProtectHome=yes` empties `/home` for the service, so the binary cannot live
  in `/home/pi`. It is installed to `/usr/local/bin`.
