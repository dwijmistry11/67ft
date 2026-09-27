# 67ft — Chrome extension

The same idea as the server in this repo, moved into the browser: request the
page as a crawler, and refuse to run the publisher's JavaScript over the answer.

Nothing is proxied. The tab still loads the real URL from the real origin, so
relative assets, character encodings and images work without any of the HTML
rewriting the server build has to do.

## Install

1. Open `chrome://extensions`
2. Turn on **Developer mode** (top right)
3. **Load unpacked** → choose this `extension/` directory

## Use

| | |
|---|---|
| Toolbar button | Reload the current tab as a crawler. Click again to go back. |
| `Alt+Shift+R` | The same toggle, without the mouse. |
| Right-click → *Always 67ft this site* | Add the site to the auto list. |
| Right-click → *67ft this page* | The toolbar button, from the page. |

A violet **ON** badge means the tab is disguised.

Sites on the auto list are handled by a persistent, domain-scoped rule rather
than a per-tab one, so the disguise is on for the very first request of the
navigation and the publisher never sees an undisguised visit at all.

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

## What it does not do

**Reverse DNS still wins.** Major publishers verify crawlers against the source
IP, and no header can answer that. This is the same ceiling the server build
documents, and neither version raises it.

**Sites that render only on the client come up blank.** If the article is not in
the server's HTML there is nothing for a crawler to have been shown.

**An installed service worker can answer the navigation from its own cache**,
never letting the request reach the network where our headers apply. Clear the
site's data once if a site seems stuck, or put it on the auto list.

**Only this browser, on this machine.** The Rust server in the parent directory
is still the answer for a phone or a tablet — and it caches, which this does
not.

**Do not expect to publish it.** The Chrome Web Store removes paywall
circumvention extensions. Loading it unpacked, or handing someone a zip, is the
distribution story.

## Files

| | |
|---|---|
| `manifest.json` | MV3 manifest. Chrome 116+, for `requestDomains`. |
| `profiles.js` | Header profiles, settings, and the rule they compile to. |
| `background.js` | Rule lifecycle: per-tab toggle, auto-site rules, injection. |
| `reader.js` | The DOM pass. |
| `reader.css` | Scroll and selection unlocking. |
| `options.html` / `options.js` | Settings page. |
| `icons/generate.py` | Regenerates the toolbar icons. |
