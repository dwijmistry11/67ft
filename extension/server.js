// Talking to a 67ft server: the other way to read a page, and the half of the
// extension that has something to report about.

// Probed in order the first time the popup opens with no server configured.
// These are the three names the server's own README ends up producing — a
// Pi-hole record, an mDNS name, and a local dev run.
export const CANDIDATES = [
  'http://67ft.lan:8080',
  'http://bliss.local:8080',
  'http://localhost:8080',
];

/** Accept what someone would actually type: bare host, trailing slash, no scheme. */
export function normalizeServer(raw) {
  let s = (raw || '').trim();
  if (!s) return '';
  if (!/^https?:\/\//i.test(s)) s = `http://${s}`;
  return s.replace(/\/+$/, '');
}

/**
 * The proxied form of a target URL.
 *
 * encodeURIComponent, to match the bookmarklet the server's landing page
 * builds: axum's Path extractor percent-decodes the wildcard segment once, so
 * encoding here and decoding there round-trips a URL that itself contains an
 * encoded character.
 */
export function proxiedUrl(server, target) {
  return `${normalizeServer(server)}/${encodeURIComponent(target)}`;
}

/** True when a tab is already showing a page this server proxied. */
export function isProxiedPage(server, url) {
  const base = normalizeServer(server);
  if (!base || !url?.startsWith(`${base}/`)) return false;
  // The landing page is the server's own, not an article it fetched.
  return url.length > base.length + 1;
}

/** The article a proxied URL is showing, for round-tripping back out of it. */
export function unproxyUrl(server, url) {
  if (!isProxiedPage(server, url)) return null;
  const rest = url.slice(normalizeServer(server).length + 1);
  try {
    return decodeURIComponent(rest);
  } catch {
    return rest; // already-decoded path, which the server also accepts
  }
}

/** Reachability and round-trip time. Never throws. */
export async function checkHealth(server, timeoutMs = 2500) {
  const base = normalizeServer(server);
  if (!base) return { ok: false, error: 'no server configured' };

  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  const started = performance.now();
  try {
    const res = await fetch(`${base}/health`, { signal: ctl.signal, cache: 'no-store' });
    const ms = Math.round(performance.now() - started);
    if (!res.ok) return { ok: false, ms, error: `HTTP ${res.status}` };
    return { ok: true, ms, body: (await res.text()).trim() };
  } catch (e) {
    return { ok: false, error: e.name === 'AbortError' ? 'timed out' : 'unreachable' };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Fetch a page through the server without navigating, to report what happened:
 * status, size, time, and whether the server answered from its cache.
 */
export async function testFetch(server, target, timeoutMs = 30000) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  const started = performance.now();
  try {
    const res = await fetch(proxiedUrl(server, target), { signal: ctl.signal, cache: 'no-store' });
    const body = await res.arrayBuffer();
    return {
      ok: res.ok,
      status: res.status,
      ms: Math.round(performance.now() - started),
      bytes: body.byteLength,
      // Set by the server on every response; "hit" means it never left the box.
      cache: res.headers.get('x-67ft-cache') || 'unknown',
    };
  } catch (e) {
    return { ok: false, error: e.name === 'AbortError' ? 'timed out' : 'unreachable' };
  } finally {
    clearTimeout(timer);
  }
}

/** First candidate that answers /health, for the case where none is set yet. */
export async function discoverServer(extra = []) {
  for (const candidate of [...extra, ...CANDIDATES]) {
    const health = await checkHealth(candidate, 1200);
    if (health.ok) return { server: candidate, ...health };
  }
  return null;
}
