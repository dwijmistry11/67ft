use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

use crate::AppState;

/// Response headers we REMOVE from upstream before sending to client.
/// These would block rendering, track the user, or misdescribe our body.
const STRIP_RESPONSE_HEADERS: &[&str] = &[
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
    "x-xss-protection",
    "strict-transport-security",
    "x-robots-tag",
    "report-to",
    "nel",
    "set-cookie", // don't leak site cookies to our domain
    "link",       // can contain preload hints that break things
    // reqwest already decoded the body, and the HTML path rewrites it, so the
    // upstream framing headers no longer describe the bytes we actually send.
    // Forwarding a stale content-length makes hyper panic on the mismatch.
    "content-length",
    "content-encoding",
];

/// Maximum redirect hops we will follow ourselves.
const MAX_REDIRECTS: usize = 10;

/// A response we are willing to serve again without refetching.
#[derive(Clone)]
pub struct CachedPage {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct Entry {
    page: CachedPage,
    stored: Instant,
}

/// A small in-memory page cache, bounded by both age and total bytes.
///
/// Re-reading an article, or following a link and going back, previously
/// refetched and re-rewrote the whole page every time.
pub struct Cache {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    max_bytes: usize,
}

impl Cache {
    pub fn new(ttl_secs: u64, max_mb: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_bytes: max_mb.saturating_mul(1024 * 1024),
        }
    }

    fn enabled(&self) -> bool {
        !self.ttl.is_zero() && self.max_bytes > 0
    }

    fn get(&self, key: &str) -> Option<CachedPage> {
        if !self.enabled() {
            return None;
        }
        let mut entries = self.entries.lock().ok()?;
        match entries.get(key) {
            Some(e) if e.stored.elapsed() < self.ttl => Some(e.page.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn put(&self, key: String, page: CachedPage) {
        if !self.enabled() || page.body.len() > self.max_bytes {
            return;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };

        entries.retain(|_, e| e.stored.elapsed() < self.ttl);

        // Evict oldest first until the new entry fits in the budget.
        let mut used: usize = entries.values().map(|e| e.page.body.len()).sum();
        while used + page.body.len() > self.max_bytes {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = entries.remove(&oldest) {
                used = used.saturating_sub(removed.page.body.len());
            }
        }

        entries.insert(key, Entry { page, stored: Instant::now() });
    }
}

/// Build an axum response from a page, cached or fresh.
fn build_response(page: &CachedPage, cache_hit: bool) -> Response {
    let mut builder = Response::builder().status(page.status);
    let headers = builder.headers_mut().unwrap();
    headers.clone_from(&page.headers);
    headers.insert(
        "x-67ft-cache",
        HeaderValue::from_static(if cache_hit { "hit" } else { "miss" }),
    );
    builder
        .body(Body::from(page.body.clone()))
        .unwrap()
        .into_response()
}

/// True if an address must never be reached through the proxy.
///
/// Without this the service is an open relay into whatever network it runs on:
/// a request for `http://127.0.0.1:...` or `http://192.168.1.1/` is fetched
/// from the host's own vantage point, and on a cloud box
/// `http://169.254.169.254/` reaches the instance metadata service.
fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0                              // 0.0.0.0/8
                || (o[0] == 100 && (o[1] & 0xc0) == 64)   // 100.64.0.0/10 CGNAT
                || o[0] >= 240                            // 240.0.0.0/4 reserved
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)     // 192.0.0.0/24 IETF
                || (o[0] == 192 && o[1] == 88 && o[2] == 99)   // 192.88.99.0/24 6to4 relay
                || (o[0] == 198 && (o[1] & 0xfe) == 18)        // 198.18.0.0/15 benchmarking
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            if (seg[0] & 0xfe00) == 0xfc00      // fc00::/7  unique local
                || (seg[0] & 0xffc0) == 0xfe80  // fe80::/10 link local
                || (seg[0] & 0xffc0) == 0xfec0  // fec0::/10 site local (deprecated)
                || seg[0] == 0x2001 && seg[1] == 0x0db8 // 2001:db8::/32 documentation
                || (seg[0] & 0xff00) == 0x0100  // 100::/8   discard-only
            {
                return true;
            }
            // Any address that embeds an IPv4 address must be judged on that
            // address: ::a.b.c.d, ::ffff:a.b.c.d, NAT64 and 6to4 all reach v4.
            if let Some(v4) = embedded_ipv4(v6) {
                return is_blocked_ip(&IpAddr::V4(v4));
            }
            false
        }
    }
}

/// Pull an IPv4 address out of the IPv6 forms that route to one.
fn embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let seg = v6.segments();
    let o = v6.octets();

    // ::ffff:a.b.c.d (mapped) and ::a.b.c.d (compatible)
    if seg[0..5] == [0, 0, 0, 0, 0] && (seg[5] == 0xffff || seg[5] == 0) {
        let v4 = std::net::Ipv4Addr::new(o[12], o[13], o[14], o[15]);
        if !v4.is_unspecified() {
            return Some(v4);
        }
    }
    // ::ffff:0:a.b.c.d (translated)
    if seg[0..4] == [0, 0, 0, 0] && seg[4] == 0xffff && seg[5] == 0 {
        return Some(std::net::Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 64:ff9b::/96 and 64:ff9b:1::/48 NAT64
    if seg[0] == 0x0064 && seg[1] == 0xff9b {
        return Some(std::net::Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 2002:a.b.c.d::/16 6to4
    if seg[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

/// A DNS resolver that refuses to return non-public addresses.
///
/// `ensure_public_host` checks before the request is made, but reqwest would
/// then resolve the name again independently, leaving a window in which a
/// hostile domain can answer with a private address the second time (DNS
/// rebinding). reqwest dials exactly the addresses this returns, so filtering
/// here closes that window.
pub struct GuardedResolver {
    allow_private: bool,
}

impl GuardedResolver {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();

        Box::pin(async move {
            let resolved: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();

            let addrs: Vec<std::net::SocketAddr> = if allow_private {
                resolved
            } else {
                resolved
                    .into_iter()
                    .filter(|a| !is_blocked_ip(&a.ip()))
                    .collect()
            };

            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The host and port a URL will actually be dialled on.
///
/// This uses the same parser reqwest does. Hand-rolled parsing disagreed with
/// it on inputs such as `http://127.0.0.1\\@evil.example.com/`, where the
/// spec treats the backslash as ending the authority: the guard checked
/// `evil.example.com` while the request went to loopback.
fn split_host_port(raw: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(raw).ok()?;
    let host = match parsed.host()? {
        url::Host::Domain(d) => d.to_string(),
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => a.to_string(),
    };
    Some((host, parsed.port_or_known_default()?))
}

/// Resolve a target host and refuse anything that points inside the network.
async fn ensure_public_host(url: &str) -> Result<(), FetchError> {
    let (host, port) = split_host_port(url)
        .ok_or_else(|| FetchError::Blocked("could not parse host from URL".into()))?;

    if host.is_empty() {
        return Err(FetchError::Blocked("empty host".into()));
    }

    let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| FetchError::Request(format!("DNS lookup for {host} failed: {e}")))?
        .collect();

    if addrs.is_empty() {
        return Err(FetchError::Request(format!("{host} did not resolve")));
    }
    for addr in &addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(FetchError::Blocked(format!(
                "{host} resolves to {}, which is not a public address",
                addr.ip()
            )));
        }
    }
    Ok(())
}

/// Resolve a Location header against the URL it came from.
fn resolve_redirect(base: &str, location: &str) -> String {
    let loc = location.trim();
    if loc.is_empty() {
        return base.to_string();
    }

    // Scheme-relative: //host/path
    if let Some(rest) = loc.strip_prefix("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }
    // Absolute, and the scheme test must be case-insensitive.
    if has_http_scheme(loc) {
        return loc.to_string();
    }

    let origin = extract_origin(base);
    if loc.starts_with('/') {
        return format!("{origin}{loc}");
    }
    // A bare query or fragment keeps the current path.
    if loc.starts_with('?') || loc.starts_with('#') {
        let path_only = base.split(['?', '#']).next().unwrap_or(base);
        return format!("{path_only}{loc}");
    }
    // Relative to the current directory, with "." and ".." resolved.
    resolve_relative(&document_base(base), loc)
}

/// Read a response body, refusing anything over `max_bytes`.
///
/// The previous code buffered whole bodies with no ceiling, so one large file
/// could exhaust memory on a small host.
async fn read_body_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, FetchError> {
    if let Some(len) = response.content_length() {
        if len as usize > max_bytes {
            return Err(FetchError::TooLarge(max_bytes));
        }
    }
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| FetchError::Body(e.to_string()))?
    {
        if buf.len() + chunk.len() > max_bytes {
            return Err(FetchError::TooLarge(max_bytes));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Follow redirects ourselves, checking every hop against the SSRF guard.
///
/// reqwest's own redirect policy cannot do the async DNS lookup each hop needs,
/// so a public URL could otherwise redirect straight to an internal address.
async fn fetch_guarded(
    state: &Arc<AppState>,
    url: &str,
) -> Result<(reqwest::Response, String), FetchError> {
    let mut current = url.to_string();

    for _ in 0..=MAX_REDIRECTS {
        if !state.config.allow_private_hosts {
            ensure_public_host(&current).await?;
        }
        debug!("fetching: {current}");

        let response = state
            .client
            .get(&current)
            .header("User-Agent", &state.config.user_agent)
            .header("X-Forwarded-For", &state.config.forwarded_for)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            // Pretend we came from Google search
            .header("Referer", "https://www.google.com/")
            .send()
            .await
            .map_err(|e| FetchError::Request(e.to_string()))?;

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|l| resolve_redirect(&current, l));

            if let Some(next) = location {
                current = next;
                continue;
            }
        }
        return Ok((response, current));
    }
    Err(FetchError::Request("too many redirects".into()))
}

/// Fetch a URL as Googlebot, optionally rewrite links so navigation stays proxied.
/// If `raw` is true, returns the HTML unmodified (no link rewriting).
pub async fn fetch_and_rewrite(
    state: &Arc<AppState>,
    url: &str,
    raw: bool,
) -> Result<Response, FetchError> {
    let cache_key = if raw { format!("raw:{url}") } else { url.to_string() };
    if let Some(page) = state.cache.get(&cache_key) {
        debug!("cache hit: {url}");
        return Ok(build_response(&page, true));
    }

    // `final_url` is the URL after redirects, which is what relative links and
    // the injected <base> must resolve against.
    let (response, final_url) = fetch_guarded(state, url).await?;

    let status = response.status();
    let upstream_headers = response.headers().clone();

    // Determine content-type
    let content_type = upstream_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();

    // For non-HTML content (images, CSS, fonts etc.), just stream it through as-is.
    let is_html = content_type.contains("text/html")
        || content_type.contains("application/xhtml+xml");
    let max_bytes = state.config.max_body_mb.saturating_mul(1024 * 1024);

    if !is_html || raw {
        let body_bytes = read_body_capped(response, max_bytes).await?;

        let mut headers = HeaderMap::new();
        forward_headers(&upstream_headers, &mut headers);
        headers.insert(
            "content-type",
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        );

        let page = CachedPage {
            status: status_convert(status),
            headers,
            body: body_bytes,
        };
        if page.status.is_success() {
            state.cache.put(cache_key, page.clone());
        }
        return Ok(build_response(&page, false));
    }

    // --- HTML path ---
    let html_bytes = read_body_capped(response, max_bytes).await?;

    let html = decode_html(&html_bytes, &content_type);
    let rewritten = rewrite_html(&html, &final_url);

    let mut headers = HeaderMap::new();
    forward_headers(&upstream_headers, &mut headers);
    // Set this AFTER forwarding: the upstream content-type names the source
    // charset, but we have transcoded the body to UTF-8, so forwarding it
    // would label the response with an encoding it no longer uses.
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );

    let page = CachedPage {
        status: status_convert(status),
        headers,
        body: rewritten.into_bytes(),
    };
    if page.status.is_success() {
        state.cache.put(cache_key, page.clone());
    }
    Ok(build_response(&page, false))
}

/// Decode a response body to UTF-8 using the charset the page declares.
///
/// `String::from_utf8_lossy` alone replaced every non-ASCII byte of a
/// legacy-encoded page with U+FFFD.
fn decode_html(bytes: &[u8], content_type: &str) -> String {
    let label = charset_from_content_type(content_type)
        .or_else(|| charset_from_meta(bytes))
        .unwrap_or_else(|| "utf-8".to_string());

    let encoding = encoding_rs::Encoding::for_label(label.as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    let (decoded, _, _) = encoding.decode(bytes);
    decoded.into_owned()
}

/// Pull `charset=` out of a Content-Type header value.
fn charset_from_content_type(content_type: &str) -> Option<String> {
    let pos = find_ci(content_type, "charset=")?;
    let raw = content_type[pos + 8..].trim();
    let value = raw
        .split(';')
        .next()?
        .trim()
        .trim_matches(['"', '\''].as_slice());
    (!value.is_empty()).then(|| value.to_string())
}

/// Look for a <meta charset> declaration near the top of the document.
///
/// Only the first 2 KiB are inspected, which is where the spec requires the
/// declaration to appear.
fn charset_from_meta(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(2048)];
    let text = String::from_utf8_lossy(head);
    let mut rest: &str = &text;

    while let Some(pos) = find_ci(rest, "<meta") {
        let after = &rest[pos..];
        let end = after.find('>').unwrap_or(after.len());
        let tag = &after[..end];

        // <meta charset="utf-8">
        if let Some(c) = find_ci(tag, "charset") {
            let tail = tag[c + 7..].trim_start();
            if let Some(tail) = tail.strip_prefix('=') {
                let v = tail.trim().trim_matches(['"', '\''].as_slice());
                let v: String = v
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '/' && *c != '>')
                    .collect();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        rest = &after[end.min(after.len())..];
        if rest.is_empty() {
            break;
        }
        rest = &rest[1.min(rest.len())..];
    }
    None
}

/// The directory a document's relative URLs resolve against, with a trailing
/// slash. "https://e.com/blog/post/page?x=1" becomes "https://e.com/blog/post/".
fn document_base(url: &str) -> String {
    let no_frag = url.split(['?', '#']).next().unwrap_or(url);
    let origin = extract_origin(no_frag);
    let path = match no_frag.find("://") {
        Some(i) => {
            let rest = &no_frag[i + 3..];
            match rest.find('/') {
                Some(p) => &rest[p..],
                None => "/",
            }
        }
        None => "/",
    };
    let dir = match path.rfind('/') {
        Some(p) => &path[..p + 1],
        None => "/",
    };
    format!("{origin}{dir}")
}

/// Join a relative reference onto a base directory, resolving "." and "..".
fn resolve_relative(base_dir: &str, rel: &str) -> String {
    let origin = extract_origin(base_dir);
    let base_path = &base_dir[origin.len()..];

    let joined = format!("{base_path}{rel}");
    let mut out: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let path = out.join("/");
    if path.starts_with('/') {
        format!("{origin}{path}")
    } else {
        format!("{origin}/{path}")
    }
}

/// ASCII case-insensitive substring search, returning a byte index into `hay`.
///
/// Needles are always ASCII, and an ASCII byte can never appear inside a
/// multi-byte UTF-8 sequence, so a hit is always on a char boundary.
fn find_ci(hay: &str, needle_lower: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

/// Extract scheme + host from a URL, e.g. "https://www.nytimes.com".
///
/// The authority ends at the first '/', '?' or '#', and any userinfo is
/// dropped so credentials never reach the rewritten page.
fn extract_origin(url: &str) -> String {
    let Some(after_scheme) = url.find("://") else {
        return url.to_string();
    };
    let scheme = &url[..after_scheme];
    let rest = &url[after_scheme + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut authority = &rest[..end];
    if let Some(at) = authority.rfind('@') {
        authority = &authority[at + 1..];
    }
    format!("{scheme}://{authority}")
}

/// Elements whose contents are raw text rather than markup. Copied through
/// untouched, so a "<script" written inside a <textarea> is not taken for a tag.
const RAW_TEXT_ELEMENTS: [&str; 3] = ["style", "textarea", "title"];

/// Rewrite HTML into a static reader view.
///
/// One pass handles everything: scripts are dropped, <base> removed,
/// <noscript> unwrapped, and <a>/<form> targets proxied. Earlier versions ran a
/// separate substring scan per concern, which mis-read comments and attribute
/// values as markup and was quadratic on some inputs.
fn rewrite_html(html: &str, original_url: &str) -> String {
    let base = document_base(original_url);
    let (body, head_end) = transform(html, &base);
    let base_tag = format!(r#"<base href="{base}">"#);

    match head_end {
        Some(at) => {
            let mut out = String::with_capacity(body.len() + base_tag.len() + 1);
            out.push_str(&body[..at]);
            out.push('\n');
            out.push_str(&base_tag);
            out.push_str(&body[at..]);
            out
        }
        None => format!("{base_tag}\n{body}"),
    }
}

/// Single linear pass over the document.
///
/// Returns the rewritten HTML and the offset just past the opening <head> tag,
/// which is where the caller injects the base tag.
fn transform(html: &str, base: &str) -> (String, Option<usize>) {
    let mut out = String::with_capacity(html.len() + 256);
    let mut head_end: Option<usize> = None;
    let bytes = html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'<' {
                i += 1;
            }
            out.push_str(&html[start..i]);
            continue;
        }

        let rest = &html[i..];

        // Comments are data, not markup: copy them verbatim. Treating a
        // "<script" or "<base" inside one as a tag used to eat the "-->" and
        // swallow the rest of the page.
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map(|e| e + 3).unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            i += end;
            continue;
        }
        // Doctype, CDATA and processing instructions.
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            i += end;
            continue;
        }

        let Some(tag) = parse_tag(rest) else {
            // A stray '<' that does not begin a tag.
            out.push('<');
            i += 1;
            continue;
        };

        let raw = &rest[..tag.end];
        match tag.name.as_str() {
            // Drop the element and everything it contains.
            "script" => {
                i += tag.end + skip_raw_text(&rest[tag.end..], "script").1;
            }
            // Drop the tag, keep the contents.
            "base" | "noscript" | "/noscript" => {
                i += tag.end;
            }
            "a" => {
                out.push_str(&rewrite_tag_attr(raw, "href", base));
                i += tag.end;
            }
            "form" => {
                out.push_str(&rewrite_tag_attr(raw, "action", base));
                i += tag.end;
            }
            name => {
                out.push_str(raw);
                i += tag.end;

                if name == "head" && head_end.is_none() {
                    head_end = Some(out.len());
                } else if RAW_TEXT_ELEMENTS.contains(&name) {
                    let (text, consumed) = skip_raw_text(&rest[tag.end..], name);
                    out.push_str(text);
                    i += consumed;
                }
            }
        }
    }
    (out, head_end)
}

/// Find the end of a raw-text element's contents.
///
/// Returns the contents including the closing tag, and how many bytes of
/// `after_open` they occupy. With no closing tag the rest of the input is
/// content, which is what the HTML parsing rules say.
fn skip_raw_text<'a>(after_open: &'a str, name: &str) -> (&'a str, usize) {
    let close = format!("</{name}");
    match find_ci(after_open, &close) {
        Some(pos) => {
            let tail = &after_open[pos..];
            let end = tail.find('>').map(|e| pos + e + 1).unwrap_or(after_open.len());
            (&after_open[..end], end)
        }
        None => (after_open, after_open.len()),
    }
}

/// A parsed opening or closing tag.
struct Tag {
    /// Lowercase name, prefixed with '/' for a closing tag.
    name: String,
    /// Byte offset just past the tag's '>'.
    end: usize,
}

/// Parse a tag starting at the '<' of `s`, respecting quoted attribute values.
///
/// Returns None for a stray '<' or an unterminated tag, so the caller copies
/// the text through rather than discarding it.
fn parse_tag(s: &str) -> Option<Tag> {
    let b = s.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }
    let mut j = 1;
    let closing = b.get(j) == Some(&b'/');
    if closing {
        j += 1;
    }
    let name_start = j;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
        j += 1;
    }
    if j == name_start {
        return None;
    }
    let mut name = String::with_capacity(j - name_start + 1);
    if closing {
        name.push('/');
    }
    name.push_str(&s[name_start..j].to_ascii_lowercase());

    // Scan to the '>' that actually ends the tag. A '>' inside a quoted
    // attribute value does not end it.
    let mut quote: Option<u8> = None;
    while j < b.len() {
        let c = b[j];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(Tag { name, end: j + 1 }),
            None => {}
        }
        j += 1;
    }
    None
}

/// Rewrite one URL-bearing attribute of a tag, matching the attribute name
/// exactly so `data-href` is never mistaken for `href`.
fn rewrite_tag_attr(tag: &str, attr: &str, base: &str) -> String {
    let b = tag.as_bytes();
    let mut j = 1;
    if b.get(j) == Some(&b'/') {
        j += 1;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
        j += 1;
    }

    while j < b.len() && b[j] != b'>' {
        while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'/') {
            j += 1;
        }
        if j >= b.len() || b[j] == b'>' {
            break;
        }

        let name_start = j;
        while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'=' && b[j] != b'>' {
            j += 1;
        }
        let name = &tag[name_start..j];

        let mut k = j;
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= b.len() || b[k] != b'=' {
            // Valueless attribute.
            j = k;
            continue;
        }
        k += 1;
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= b.len() {
            break;
        }

        // Span of the value including any quotes.
        let (outer_start, outer_end, inner_start, inner_end) =
            if b[k] == b'"' || b[k] == b'\'' {
                let q = b[k];
                let inner = k + 1;
                let mut e = inner;
                while e < b.len() && b[e] != q {
                    e += 1;
                }
                (k, (e + 1).min(b.len()), inner, e)
            } else {
                let inner = k;
                let mut e = inner;
                while e < b.len() && !b[e].is_ascii_whitespace() && b[e] != b'>' {
                    e += 1;
                }
                (inner, e, inner, e)
            };

        if name.eq_ignore_ascii_case(attr) {
            // HTML strips surrounding whitespace from a URL attribute.
            let value = tag[inner_start..inner_end].trim();
            let rewritten = rewrite_single_href(value, base);
            return format!(
                "{}\"{}\"{}",
                &tag[..outer_start],
                rewritten,
                &tag[outer_end..]
            );
        }
        j = outer_end;
    }
    tag.to_string()
}

/// Given a single href value, rewrite it to go through the proxy if it's navigable.
fn rewrite_single_href(href: &str, base: &str) -> String {
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with("mailto:")
        || href.starts_with("javascript:")
        || href.starts_with("tel:")
        || href.starts_with("data:")
        || href.starts_with("ftp:")
    {
        return href.to_string();
    }

    // Protocol-relative: //cdn.example.com/x inherits our scheme.
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("/{}", encode_url(&format!("{scheme}://{rest}")));
    }

    // Absolute URL, same origin or not, keep it proxied.
    if has_http_scheme(href) {
        return format!("/{}", encode_url(href));
    }

    // Root-relative path like /article/foo
    if href.starts_with('/') {
        let origin = extract_origin(base);
        return format!("/{}", encode_url(&format!("{origin}{href}")));
    }

    // Relative path. The <base> tag resolves these to the origin, which for an
    // anchor means navigating off the proxy and straight back into the
    // paywall, so resolve and proxy it instead.
    format!("/{}", encode_url(&resolve_relative(base, href)))
}

/// True if the reference begins with an http or https scheme, in any case.
fn has_http_scheme(s: &str) -> bool {
    let lower_prefix = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    lower_prefix.starts_with("http://") || lower_prefix.starts_with("https://")
}

/// Percent-encode a URL so it survives as a single path segment.
///
/// '?' and '#' must be escaped or the browser treats the tail of the target
/// URL as the proxy's own query string and fragment, and the server never
/// sees it. '%' goes first so the encoding round-trips.
fn encode_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len() + 16);
    for ch in url.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            '"' => out.push_str("%22"),
            '\'' => out.push_str("%27"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            _ => out.push(ch),
        }
    }
    out
}

/// Copy safe response headers from upstream to our response.
fn forward_headers(upstream: &HeaderMap, dest: &mut HeaderMap) {
    for (name, value) in upstream.iter() {
        let name_str = name.as_str();
        if STRIP_RESPONSE_HEADERS.contains(&name_str) {
            continue;
        }
        if matches!(
            name_str,
            "transfer-encoding"
                | "connection"
                | "keep-alive"
                | "upgrade"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
        ) {
            continue;
        }
        if let Ok(header_name) = HeaderName::try_from(name_str) {
            dest.insert(header_name, value.clone());
        }
    }
}

/// Convert reqwest StatusCode to axum StatusCode
fn status_convert(s: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// Errors that can occur during proxying
#[derive(Debug)]
pub enum FetchError {
    Request(String),
    Body(String),
    /// Target resolved to a non-public address.
    Blocked(String),
    /// Response body exceeded the configured cap.
    TooLarge(usize),
}

impl FetchError {
    /// Status to return to the client for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            FetchError::Blocked(_) => StatusCode::FORBIDDEN,
            FetchError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Request(e) => write!(f, "request failed: {e}"),
            FetchError::Body(e) => write!(f, "reading response body failed: {e}"),
            FetchError::Blocked(e) => write!(f, "blocked: {e}"),
            FetchError::TooLarge(max) => {
                write!(f, "response exceeded the {} MB limit", max / (1024 * 1024))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://e.com/blog/post/";

    fn rw(html: &str) -> String {
        rewrite_html(html, "https://e.com/blog/post/page")
    }

    #[test]
    fn strips_scripts() {
        assert!(!rw("<p>a</p><script>x=1</script><p>b</p>").contains("x=1"));
        assert!(rw("<p>a</p><script>x=1</script><p>b</p>").contains("<p>b</p>"));
        // A '>' inside the script's own attributes must not end the tag early.
        assert!(!rw(r#"<script data-x="a>b">bad()</script><p>keep</p>"#).contains("bad()"));
        // Unquoted attribute ending in '/' is not a self-closing tag.
        let out = rw("<p>a</p><script src=/j/>alert(1)</script><p>b</p>");
        assert!(!out.contains("alert(1)"), "{out}");
        assert!(!out.contains("</script>"), "{out}");
    }

    #[test]
    fn comments_and_attributes_are_not_markup() {
        // A "<script" inside a comment used to delete the rest of the document.
        let out = rw("<p>before</p><!-- inline <script> blocks here --><p>after</p>");
        assert!(out.contains("<p>after</p>"), "{out}");
        let out = rw(r#"<p>before</p><div data-tpl="<script>"></div><p>after</p>"#);
        assert!(out.contains("<p>after</p>"), "{out}");
        // A "<base" inside a comment used to eat the comment's terminator.
        let out = rw("<p>a</p><!-- the <base tag is injected --><p>b</p>");
        assert!(out.contains("-->"), "{out}");
        assert!(out.contains("<p>b</p>"), "{out}");
    }

    #[test]
    fn raw_text_elements_are_left_alone() {
        let out = rw("<textarea><script>x</script></textarea><p>after</p>");
        assert!(out.contains("<script>x</script>"), "{out}");
        assert!(out.contains("<p>after</p>"), "{out}");
    }

    #[test]
    fn base_tag_is_the_document_directory() {
        let out = rw("<html><head></head><body></body></html>");
        assert!(out.contains(r#"<base href="https://e.com/blog/post/">"#), "{out}");
        // Existing base tags are removed.
        assert!(!rw(r#"<head><base href="/x"></head>"#).contains(r#"href="/x""#));
    }

    #[test]
    fn base_is_only_injected_after_a_real_head_tag() {
        // "<head" inside a comment, and a <header> element, are not <head>.
        let out = rw("<html><!-- <head> is next --><head><title>t</title></head><body>b</body>");
        let comment_end = out.find("-->").unwrap();
        let base_at = out.find("<base ").unwrap();
        assert!(base_at > comment_end, "base injected inside the comment: {out}");
        let out = rw("<html><body><header>hi</header></body></html>");
        assert!(out.starts_with("<base "), "{out}");
    }

    #[test]
    fn noscript_is_unwrapped() {
        let out = rw("<noscript><img src=a></noscript>");
        assert!(out.contains("<img src=a>"), "{out}");
        assert!(!out.contains("noscript"), "{out}");
    }

    #[test]
    fn anchors_are_rewritten_precisely() {
        // exact attribute match: data-href must not be touched
        let out = rewrite_tag_attr(r#"<a data-href="/track" href="/article">"#, "href", BASE);
        assert!(out.contains(r#"data-href="/track""#), "{out}");
        assert!(out.contains(r#"href="/https://e.com/article""#), "{out}");
        // '>' inside another attribute
        let out = rewrite_tag_attr(r#"<a title="a>b" href="/article">"#, "href", BASE);
        assert!(out.contains(r#"href="/https://e.com/article""#), "{out}");
        // unquoted value
        let out = rewrite_tag_attr("<a href=/article>", "href", BASE);
        assert!(out.contains(r#"href="/https://e.com/article""#), "{out}");
        // leading whitespace, which HTML strips
        let out = rewrite_tag_attr(r#"<a href=" /article">"#, "href", BASE);
        assert!(out.contains(r#"href="/https://e.com/article""#), "{out}");
    }

    #[test]
    fn relative_anchors_stay_proxied() {
        let out = rw(r#"<a href="page2">x</a>"#);
        assert!(out.contains(r#"href="/https://e.com/blog/post/page2""#), "{out}");
    }

    #[test]
    fn form_actions_are_rewritten() {
        let out = rw(r#"<form action="/search"><input name=q></form>"#);
        assert!(out.contains(r#"action="/https://e.com/search""#), "{out}");
    }

    #[test]
    fn unterminated_markup_is_not_discarded() {
        // None of these may panic or silently drop the tail.
        assert!(rw(r#"<p>keep</p><a href="/foo"#).contains("keep"));
        assert!(rw("<p>keep</p><div attr='").contains("keep"));
    }

    #[test]
    fn scanner_is_linear() {
        // "<noscript>" repeated with no closing tag was quadratic: a 1 MB page
        // took over twelve seconds.
        let html = "<noscript>".repeat(80_000);
        let start = std::time::Instant::now();
        let _ = rewrite_html(&html, "https://e.com/p");
        assert!(start.elapsed().as_secs() < 2, "took {:?}", start.elapsed());
    }

    #[test]
    fn document_base_keeps_the_path() {
        assert_eq!(document_base("https://e.com/blog/post/page"), "https://e.com/blog/post/");
        assert_eq!(document_base("https://e.com/page"), "https://e.com/");
        assert_eq!(document_base("https://e.com"), "https://e.com/");
        assert_eq!(document_base("https://e.com/a/b?x=1#f"), "https://e.com/a/");
    }

    #[test]
    fn origin_stops_at_the_authority() {
        assert_eq!(extract_origin("https://e.com?utm=1"), "https://e.com");
        assert_eq!(extract_origin("https://e.com#frag"), "https://e.com");
        // Credentials must never reach the rewritten page.
        assert_eq!(extract_origin("https://user:s3cret@e.com/x"), "https://e.com");
    }

    #[test]
    fn resolves_relative_references() {
        assert_eq!(resolve_relative(BASE, "page2"), "https://e.com/blog/post/page2");
        assert_eq!(resolve_relative(BASE, "./page2"), "https://e.com/blog/post/page2");
        assert_eq!(resolve_relative(BASE, "../other"), "https://e.com/blog/other");
        assert_eq!(resolve_relative(BASE, "../../../../x"), "https://e.com/x");
    }

    #[test]
    fn resolves_redirect_targets() {
        let b = "https://e.com/a/b";
        assert_eq!(resolve_redirect(b, "https://x.com/y"), "https://x.com/y");
        assert_eq!(resolve_redirect(b, "HTTPS://other.com/z"), "HTTPS://other.com/z");
        assert_eq!(resolve_redirect(b, "/c"), "https://e.com/c");
        assert_eq!(resolve_redirect(b, "//cdn.e.com/z"), "https://cdn.e.com/z");
        assert_eq!(resolve_redirect(b, "c"), "https://e.com/a/c");
        assert_eq!(resolve_redirect(b, "?page=2"), "https://e.com/a/b?page=2");
        assert_eq!(resolve_redirect("https://e.com", "foo"), "https://e.com/foo");
        assert_eq!(resolve_redirect("https://e.com/a/b?next=/x/y", "c"), "https://e.com/a/c");
    }

    #[test]
    fn blocks_non_public_addresses() {
        for ip in [
            "127.0.0.1", "10.0.0.5", "192.168.1.1", "172.16.0.1", "169.254.169.254",
            "0.0.0.0", "100.64.0.1", "198.18.0.1", "192.0.0.1", "192.88.99.1",
            "::1", "fe80::1", "fc00::1", "fec0::1", "2001:db8::1", "100::1",
            "::ffff:127.0.0.1", "::127.0.0.1", "::ffff:0:127.0.0.1",
            "64:ff9b::7f00:1", "2002:7f00:1::1",
        ] {
            assert!(is_blocked_ip(&ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_blocked_ip(&ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn host_parsing_matches_the_http_client() {
        assert_eq!(split_host_port("https://e.com/a"), Some(("e.com".into(), 443)));
        assert_eq!(split_host_port("http://e.com:8080/a?b=1"), Some(("e.com".into(), 8080)));
        assert_eq!(split_host_port("http://[::1]:9000/a"), Some(("::1".into(), 9000)));
        assert_eq!(split_host_port("http://user@127.0.0.1/a"), Some(("127.0.0.1".into(), 80)));
        // A backslash ends the authority, so this is loopback, not evil.example.com.
        assert_eq!(
            split_host_port("http://127.0.0.1\\@evil.example.com/"),
            Some(("127.0.0.1".into(), 80))
        );
    }

    #[test]
    fn query_and_fragment_are_encoded() {
        assert_eq!(
            rewrite_single_href("/next?x=1&y=2#f", BASE),
            "/https://e.com/next%3Fx=1&y=2%23f"
        );
        assert_eq!(rewrite_single_href("//cdn.e.com/x.js", BASE), "/https://cdn.e.com/x.js");
        assert_eq!(rewrite_single_href("#top", BASE), "#top");
        assert_eq!(rewrite_single_href("mailto:a@b.c", BASE), "mailto:a@b.c");
    }

    #[test]
    fn decodes_legacy_charsets() {
        assert_eq!(decode_html(b"caf\xe9", "text/html; charset=iso-8859-1"), "caf\u{e9}");
        let meta = b"<html><head><meta charset=\"iso-8859-1\"></head><body>caf\xe9</body></html>";
        assert!(decode_html(meta, "text/html").contains("caf\u{e9}"));
        assert_eq!(decode_html("caf\u{e9}".as_bytes(), "text/html; charset=utf-8"), "caf\u{e9}");
    }

    #[test]
    fn parses_charset_from_content_type() {
        assert_eq!(charset_from_content_type("text/html; charset=UTF-8"), Some("UTF-8".into()));
        assert_eq!(charset_from_content_type("text/html;charset=\"gbk\""), Some("gbk".into()));
        assert_eq!(charset_from_content_type("text/html"), None);
    }

    #[test]
    fn cache_respects_ttl_and_budget() {
        let cache = Cache::new(300, 1);
        let page = CachedPage {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: vec![b'x'; 600 * 1024],
        };
        cache.put("a".into(), page.clone());
        assert!(cache.get("a").is_some());
        // The second entry does not fit alongside the first, so the older goes.
        cache.put("b".into(), page);
        assert!(cache.get("b").is_some());
        assert!(cache.get("a").is_none());

        let off = Cache::new(0, 32);
        off.put("a".into(), CachedPage {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: vec![1],
        });
        assert!(off.get("a").is_none());
    }
}
