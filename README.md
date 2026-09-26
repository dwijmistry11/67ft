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
- Strips CSP / X-Frame-Options headers that would block rendering

Because scripts are removed, interactive features such as comments, embedded
players and infinite scroll will not work. That is the trade for reliable text.

Note that major publishers verify crawlers by reverse DNS on the source IP, so a
forged user agent alone will not pass on every site.

## Run as a systemd service (Pi / Linux)

```ini
# /etc/systemd/system/67ft.service
[Unit]
Description=67ft Paywall Proxy
After=network-online.target

[Service]
Type=simple
ExecStart=/home/pi/67ft/67ft --port 8080
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now 67ft
```
