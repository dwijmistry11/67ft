# tools

Coverage testing for the proxy. `check-sites.py` fetches a list of sites
through a 67ft server and judges each response by what is in the body, because
HTTP 200 answers almost nothing: a paywall returns 200 with a teaser, a bot
wall returns 200 with a challenge, and a single-page app returns 200 with an
empty shell.

```sh
./tools/check-sites.py --list sites.txt --server http://67ft.lan:8080 --out proxy.json
./tools/check-sites.py --list sites.txt --direct                      --out direct.json
./tools/compare.py proxy.json direct.json
```

The `--direct` control run is the point. A list of sites that fail through the
proxy means nothing on its own: most of them were never readable to begin with.
What matters is the sites a plain browser could read and the proxy could not.

## Results, top 500 domains

Measured against a Pi 5 CM running the server, September 2026.

| | via 67ft | plain browser |
|---|---|---|
| readable | **55.2%** | 51.8% |

Compared site by site:

| | |
|---|---|
| both readable | 43.4% |
| neither readable | 36.4% |
| **67ft rescued it** | **11.8%** |
| **67ft broke it** | **8.4%** |

Net positive, but both tails are large, and the reasons are more useful than
the totals.

**What 67ft rescues** (59 sites) is mostly bot-blocking that the crawler
disguise walks straight past: `ebay.com`, `github.com`, `paypal.com`,
`dropbox.com` and `oracle.com` all refuse a plain request and serve a crawler.
`lemonde.fr` is the clearest win of the intended kind — a paywall directly, an
article through the proxy.

**What 67ft breaks** (42 sites) is almost entirely the ceiling the main README
already names. Sites that verify crawlers by reverse DNS see a Googlebot user
agent arriving from a home IP and refuse it outright — `cnn.com`,
`wikipedia.org`, `office.com`, `cornell.edu`. A handful more (`ovh.com`,
`skype.com`) redirect a crawler into a loop the proxy gives up on; that one is
the site's own geo-redirect misbehaving, not a cookie the proxy failed to
keep — following the same chain with a cookie jar loops identically.

The rest of the failures nobody can read either way: 33 captcha, 32 bot
challenge, 32 hard block, 32 client-rendered shells with no article in the HTML
at all.

## Results, 45 publishers and blogs

**Readable (23)** — washingtonpost.com, newyorker.com, wired.com, forbes.com,
barrons.com, thetimes.co.uk, bostonglobe.com, latimes.com, newscientist.com,
nature.com, theinformation.com, techcrunch.com, theverge.com, bbc.com,
text.npr.org, and the engineering blogs.

**Not readable (22)** — nytimes.com, wsj.com, ft.com, reuters.com, cnbc.com,
theatlantic.com and reddit.com block the server outright. bloomberg.com,
medium.com, theguardian.com, arstechnica.com and businessinsider.com answer
with a captcha. economist.com, apnews.com, science.org and quora.com serve a
Cloudflare challenge. telegraph.co.uk renders only on the client.

Roughly half the sites worth pointing this at refuse it, and no header fixes
that — the source IP is the thing being judged.

## What this means for the extension

A site that answers the server with a bot wall is frequently readable in the
browser as it stands, because the browser is a real browser. Quora is the clean
example: a Cloudflare challenge for the server, and twelve thousand characters
of article with no overlay and no scroll lock for Chrome.

So the extension probes before it sends the tab anywhere, and stays on the
original when the server cannot fetch the page. Local mode, not server mode, is
the answer for that whole category.
