# 67ft — Chrome extension

The same idea as the server in this repo, moved into the browser: request the
page as a crawler, and refuse to run the publisher's JavaScript over the answer.

It works two ways, switched from the popup:

**Local** — nothing is proxied. The tab loads the real URL from the real
origin, disguised by header rules, so relative assets, character encodings and
images work without any of the HTML rewriting the server build has to do. The
publisher sees your address.

**Server** — the tab is sent to a 67ft server, which fetches the page from its
own address and returns it rewritten. The only mode where the publisher never
learns which machine was curious, and the only one that benefits from the
server's cache. The extension then runs its reader pass over the result, which
the server cannot do for itself — so the combination reads better than either
half alone.

## Install

1. Open `chrome://extensions`
2. Turn on **Developer mode** (top right)
3. **Load unpacked** → choose this `extension/` directory

## Use

| | |
|---|---|
| Toolbar button | Opens the popup: server status, the current page, mode, diagnostics. |
| `Alt+Shift+R` | The same toggle, without the mouse. |
| Right-click → *Always 67ft this site* | Add the site to the auto list. |
| Right-click → *67ft this page* | The toolbar button, from the page. |

A violet **ON** badge means the tab is disguised locally; **SRV** means you are
reading it through the server.

### The popup

- **Server** — a live dot, the address, and the round-trip time to `/health`.
  With no server set it probes `67ft.lan`, `bliss.local` and `localhost` on
  port 8080 and keeps the first that answers; you can also type one in, scheme
  optional.
- **This page** — the site, whether you are reading it direct, as a crawler, or
  through the server, and one button that reflects that. With the disguise on,
  it reads *Turn off for this tab*; on a proxied page, *Back to the original*.
  Every state has a way back.
- **Mode** — local or server, and *Always, for this site*, which applies the
  current mode to that domain from now on.
- **Diagnostics** — fetches the current page through the server *without
  navigating*, and reports the status, size, time, and whether the server
  answered from its cache. A `hit` means the publisher was never contacted at
  all.

Sites on the auto list are handled, in local mode, by a persistent
domain-scoped rule rather than a per-tab one, so the disguise is on for the
very first request of the navigation and the publisher never sees an
undisguised visit at all. In server mode the same list redirects to the server
before the page loads, and the header rules are withdrawn — the request is not
being made from this browser, so disguising it here would mean nothing.

## Options

Right-click the toolbar icon → *Options*.

- **Disguise** — Googlebot (default), Bingbot, or *Arrived from Google*, which
  keeps your real browser identity but strips cookies and claims a search
  referral. The last one works on metered sites that grant free reads to search
  traffic while ignoring crawler claims outright.
- **Custom user agent** — overrides the profile's, leaving its cookie and
  referrer handling alone.
- **Block all scripts** — the equivalent of the server build stripping every
  `<script>`, done with a `Content-Security-Policy: script-src 'none'` response
  header instead of a tokenizer.
- **Clear overlays** — the DOM pass in `reader.js`.
- **Always on for** — one hostname per line; subdomains included.

## How it works

Two `declarativeNetRequest` rules and one content script.

**Request headers**, on the main document only, so images and stylesheets keep
loading normally as you:

- `user-agent` set to the profile's crawler
- `cookie` removed — a metered reader is a cookie, and sending it next to a
  Googlebot user agent is a contradiction the paywall resolves against you
- `referer` removed, or set to Google for the referral profile
- `x-forwarded-for` set, which the publisher's CDN almost certainly overwrites
  with your real address. Kept for parity with the server, where it is equally
  decorative.

**Response headers**: `content-security-policy` set to `script-src 'none'`.
Publishers ship single-page apps that re-render from their own router and
discard the server-rendered article, so killing scripts is what makes the
crawler's copy of the page survive to be read.

**`reader.js`** then does what a header cannot. It runs in an isolated world,
which the policy we just imposed does not govern:

- unwraps `<noscript>`, because blocking scripts via CSP is not the same as
  disabling scripting — the page still counts as script-enabled, so lazy-load
  `<img>` fallbacks would stay inert
- removes overlays that cover the viewport or name themselves as a paywall,
  but un-pins rather than deletes one that holds the article
- undoes `max-height` clamps, fade masks and `filter: blur()` over article text
- restores scrolling and text selection

### When a site refuses

Roughly one site in seven answers a crawler with a bot wall rather than an
article, and it is usually a site a normal browser reads without complaint.
Both modes now check rather than assume.

In server mode the extension asks the server for the page before sending the
tab anywhere, and stays put if the server cannot fetch it. In local mode it
looks at the page the disguise produced, and if that page is a challenge or a
block it withdraws the disguise for that site, reloads it normally, and says so
with a **!** badge. The site is set aside for the rest of the browser session
only; the popup offers to try it again.

Quora is the case this was built for: a Cloudflare challenge for the server and
for the disguise, and twelve thousand characters of article for plain Chrome.

## What it does not do

**Reverse DNS still wins.** Major publishers verify crawlers against the source
IP, and no header can answer that. This is the same ceiling the server build
documents, and neither version raises it.

**Sites that render only on the client come up blank.** If the article is not in
the server's HTML there is nothing for a crawler to have been shown.

**An installed service worker can answer the navigation from its own cache**,
never letting the request reach the network where our headers apply. Clear the
site's data once if a site seems stuck, or put it on the auto list.

**Local mode is only this browser, on this machine.** The Rust server in the
parent directory is still the answer for a phone or a tablet — which is what
server mode is for, and where the caching lives.

**Do not expect to publish it.** The Chrome Web Store removes paywall
circumvention extensions. Loading it unpacked, or handing someone a zip, is the
distribution story.

## Files

| | |
|---|---|
| `manifest.json` | MV3 manifest. Chrome 116+, for `requestDomains`. |
| `profiles.js` | Header profiles, settings, and the rule they compile to. |
| `server.js` | Talking to a 67ft server: health, discovery, URL round-tripping. |
| `popup.html` / `popup.js` | The toolbar popup: status, mode, diagnostics. |
| `background.js` | Rule lifecycle: per-tab toggle, auto-site rules, injection. |
| `reader.js` | The DOM pass. |
| `reader.css` | Scroll and selection unlocking. |
| `options.html` / `options.js` | Settings page. |
| `icons/generate.py` | Regenerates the toolbar icons. |
