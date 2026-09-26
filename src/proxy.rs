use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
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
    "x-content-type-options",
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
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
        }
    }
}

/// Split "scheme://host:port/path" into a host and a port.
fn split_host_port(url: &str) -> Option<(String, u16)> {
    let after_scheme = url.find("://")?;
    let scheme = &url[..after_scheme];
    let rest = &url[after_scheme + 3..];
    let authority_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let mut authority = &rest[..authority_end];

    // Strip any userinfo, which is not part of the host.
    if let Some(at) = authority.rfind('@') {
        authority = &authority[at + 1..];
    }

    let default_port = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };

    // IPv6 literals are bracketed: [::1]:8080
    if let Some(close) = authority.find(']') {
        let host = authority.get(1..close)?.to_string();
        let port = authority[close + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }

    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().unwrap_or(default_port))),
        None => Some((authority.to_string(), default_port)),
    }
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
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let origin = extract_origin(base);
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = origin.split(':').next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }
    if location.starts_with('/') {
        return format!("{origin}{location}");
    }
    // Relative to the current directory.
    let path_start = base.find("://").map(|i| i + 3).unwrap_or(0);
    let dir_end = base[path_start..]
        .rfind('/')
        .map(|i| path_start + i + 1)
        .unwrap_or(base.len());
    format!("{}{}", &base[..dir_end], location)
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
    let is_html = content_type.contains("text/html");
    let max_bytes = state.config.max_body_mb.saturating_mul(1024 * 1024);

    if !is_html || raw {
        let body_bytes = read_body_capped(response, max_bytes).await?;

        let mut builder = Response::builder().status(status_convert(status));

        let headers = builder.headers_mut().unwrap();
        forward_headers(&upstream_headers, headers);

        headers.insert(
            "content-type",
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        );

        return Ok(builder
            .body(Body::from(body_bytes))
            .unwrap()
            .into_response());
    }

    // --- HTML path ---
    let html_bytes = read_body_capped(response, max_bytes).await?;

    let html = String::from_utf8_lossy(&html_bytes).into_owned();
    let rewritten = rewrite_html(&html, &final_url);

    let mut builder = Response::builder()
        .status(status_convert(status))
        .header("content-type", "text/html; charset=utf-8");

    let headers = builder.headers_mut().unwrap();
    forward_headers(&upstream_headers, headers);

    Ok(builder
        .body(Body::from(rewritten))
        .unwrap()
        .into_response())
}

/// Rewrite HTML into a static reader view:
/// 1. Drop any existing <base> tag.
/// 2. Remove all <script> elements. Modern publishers ship a single-page app
///    that re-renders from its own router and API state; left in place it
///    discards the server-rendered article and shows its own 404.
/// 3. Unwrap <noscript> so lazy-loaded images become real images.
/// 4. Rewrite <a href> so navigation stays proxied.
/// 5. Inject <base href> last, so the link rewriter cannot mangle it.
fn rewrite_html(html: &str, original_url: &str) -> String {
    let origin = extract_origin(original_url);

    let html = remove_existing_base(html);
    let html = strip_scripts(&html);
    let html = unwrap_noscript(&html);
    let html = rewrite_anchor_hrefs(&html, &origin);

    let base_tag = format!(r#"<base href="{origin}/">"#);
    inject_after_head(&html, &base_tag)
}

/// ASCII case-insensitive substring search, returning a byte index into `hay`.
///
/// Needles are always ASCII, and an ASCII byte can never appear inside a
/// multi-byte UTF-8 sequence, so a hit is always on a char boundary. This
/// replaces the previous `to_lowercase()` approach, which allocated a copy of
/// the document per call and produced indices that did not line up with the
/// original whenever lowercasing changed a character's byte length.
fn find_ci(hay: &str, needle_lower: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

/// True if the byte after a tag name ends the name (whitespace, '>' or '/').
fn is_tag_name_end(b: Option<&u8>) -> bool {
    matches!(b, Some(c) if c.is_ascii_whitespace() || *c == b'>' || *c == b'/')
}

/// Extract scheme + host from a URL, e.g. "https://www.nytimes.com"
fn extract_origin(url: &str) -> String {
    if let Some(after_scheme) = url.find("://") {
        let rest = &url[after_scheme + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        let host = &rest[..host_end];
        let scheme = &url[..after_scheme];
        return format!("{scheme}://{host}");
    }
    url.to_string()
}

/// Remove existing <base ...> tags
fn remove_existing_base(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = find_ci(rest, "<base") {
        let after = &rest[start..];
        if !is_tag_name_end(after.as_bytes().get(5)) {
            // e.g. "<basefont" — not a <base> tag, emit and continue past it.
            result.push_str(&rest[..start + 5]);
            rest = &rest[start + 5..];
            continue;
        }
        result.push_str(&rest[..start]);
        match after.find('>') {
            Some(end) => rest = &after[end + 1..],
            None => return result, // unterminated tag: drop the remainder
        }
    }
    result.push_str(rest);
    result
}

/// Remove every <script> element, opening tag, contents and closing tag.
fn strip_scripts(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = find_ci(rest, "<script") {
        let after = &rest[start..];
        if !is_tag_name_end(after.as_bytes().get(7)) {
            out.push_str(&rest[..start + 7]);
            rest = &rest[start + 7..];
            continue;
        }
        out.push_str(&rest[..start]);

        let Some(gt) = after.find('>') else {
            return out; // unterminated opening tag: drop the remainder
        };

        // Self-closing <script ... /> has no body to skip.
        if after[..gt].ends_with('/') {
            rest = &after[gt + 1..];
            continue;
        }

        let body = &after[gt + 1..];
        match find_ci(body, "</script") {
            Some(close) => {
                let tail = &body[close..];
                match tail.find('>') {
                    Some(e) => rest = &tail[e + 1..],
                    None => return out,
                }
            }
            // No closing tag: the rest of the document is script content.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Strip <noscript> and </noscript> tags while keeping their contents.
/// Publishers put the real <img> for lazy-loaded media inside <noscript>,
/// so unwrapping restores images now that no script runs.
fn unwrap_noscript(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    loop {
        let open = find_ci(rest, "<noscript");
        let close = find_ci(rest, "</noscript");
        let (start, name_len) = match (open, close) {
            (Some(o), Some(c)) if o < c => (o, 9),
            (Some(o), None) => (o, 9),
            (_, Some(c)) => (c, 10),
            (None, None) => break,
        };

        let after = &rest[start..];
        if !is_tag_name_end(after.as_bytes().get(name_len)) {
            out.push_str(&rest[..start + name_len]);
            rest = &rest[start + name_len..];
            continue;
        }
        out.push_str(&rest[..start]);
        match after.find('>') {
            Some(end) => rest = &after[end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Rewrite href on <a> tags only, so same-origin navigation stays proxied.
///
/// Previously this matched any `href=` in the document, which sent every
/// stylesheet and preload through the proxy and rewrote our own <base> tag.
fn rewrite_anchor_hrefs(html: &str, origin: &str) -> String {
    let mut out = String::with_capacity(html.len() + 512);
    let mut rest = html;

    while let Some(start) = find_ci(rest, "<a") {
        let after = &rest[start..];
        // Only an "<a" followed by whitespace is an anchor tag; "<article" is not.
        if !matches!(after.as_bytes().get(2), Some(c) if c.is_ascii_whitespace()) {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            continue;
        }
        let Some(gt) = after.find('>') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&rewrite_href_in_tag(&after[..=gt], origin));
        rest = &after[gt + 1..];
    }
    out.push_str(rest);
    out
}

/// Rewrite the href attribute inside a single opening tag.
fn rewrite_href_in_tag(tag: &str, origin: &str) -> String {
    let Some(hpos) = find_ci(tag, "href=") else {
        return tag.to_string();
    };
    let after = &tag[hpos + 5..];

    let (quote, value_start) = match after.as_bytes().first() {
        Some(b'"') => ('"', &after[1..]),
        Some(b'\'') => ('\'', &after[1..]),
        // Unquoted attribute value: leave the tag untouched rather than guess.
        _ => return tag.to_string(),
    };

    // An unterminated value used to index past the end of the string and panic.
    let Some(end) = value_start.find(quote) else {
        return tag.to_string();
    };

    let rewritten = rewrite_single_href(&value_start[..end], origin);
    format!(
        "{}href={}{}{}{}",
        &tag[..hpos],
        quote,
        rewritten,
        quote,
        &value_start[end + 1..]
    )
}

/// Given a single href value, rewrite it to go through the proxy if it's navigable.
fn rewrite_single_href(href: &str, origin: &str) -> String {
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

    // Absolute URL, same origin or not — keep it proxied.
    if href.starts_with("http://") || href.starts_with("https://") {
        return format!("/{}", encode_url(href));
    }

    // Protocol-relative: //cdn.example.com/x inherits our scheme.
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = origin.split(':').next().unwrap_or("https");
        return format!("/{}", encode_url(&format!("{scheme}://{rest}")));
    }

    // Root-relative path like /article/foo
    if href.starts_with('/') {
        return format!("/{}", encode_url(&format!("{origin}{href}")));
    }

    // Relative path — the <base> tag handles these.
    href.to_string()
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

/// Inject a string immediately after the opening <head> tag (or prepend).
fn inject_after_head(html: &str, injection: &str) -> String {
    if let Some(head_pos) = find_ci(html, "<head") {
        if let Some(close) = html[head_pos..].find('>') {
            let insert_at = head_pos + close + 1;
            let mut out = String::with_capacity(html.len() + injection.len() + 2);
            out.push_str(&html[..insert_at]);
            out.push('\n');
            out.push_str(injection);
            out.push_str(&html[insert_at..]);
            return out;
        }
    }
    format!("{injection}\n{html}")
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

    const ORIGIN: &str = "https://e.com";

    #[test]
    fn strips_scripts_including_unterminated() {
        assert_eq!(strip_scripts("<p>a</p><script>x=1</script><p>b</p>"), "<p>a</p><p>b</p>");
        assert_eq!(strip_scripts(r#"<script src="x.js"></script>ok"#), "ok");
        assert_eq!(strip_scripts("<script type=module>let a='</p>'</script>hi"), "hi");
        assert_eq!(strip_scripts("keep<scripting>this</scripting>"), "keep<scripting>this</scripting>");
        assert_eq!(strip_scripts("a<script>oops"), "a");
    }

    #[test]
    fn unterminated_href_quote_does_not_panic() {
        // This input used to panic with an out-of-bounds slice.
        let out = rewrite_anchor_hrefs(r#"<a href="/foo"#, ORIGIN);
        assert_eq!(out, r#"<a href="/foo"#);
    }

    #[test]
    fn only_anchors_are_rewritten() {
        let html = r#"<link rel="stylesheet" href="/s.css"><a href="/art">x</a>"#;
        let out = rewrite_anchor_hrefs(html, ORIGIN);
        assert!(out.contains(r#"<link rel="stylesheet" href="/s.css">"#));
        assert!(out.contains(r#"href="/https://e.com/art""#));
        // <article> must not be mistaken for an anchor
        assert_eq!(rewrite_anchor_hrefs("<article>x</article>", ORIGIN), "<article>x</article>");
    }

    #[test]
    fn base_tag_survives_link_rewriting() {
        let html = r#"<html><head></head><body><a href="/a">x</a></body></html>"#;
        let out = rewrite_html(html, "https://e.com/post/1");
        assert!(out.contains(r#"<base href="https://e.com/">"#), "{out}");
    }

    #[test]
    fn query_and_fragment_are_encoded() {
        assert_eq!(
            rewrite_single_href("/next?x=1&y=2#f", ORIGIN),
            "/https://e.com/next%3Fx=1&y=2%23f"
        );
        assert_eq!(
            rewrite_single_href("//cdn.e.com/x.js", ORIGIN),
            "/https://cdn.e.com/x.js"
        );
        assert_eq!(rewrite_single_href("#top", ORIGIN), "#top");
        assert_eq!(rewrite_single_href("rel/path", ORIGIN), "rel/path");
    }

    #[test]
    fn lowercase_length_change_does_not_corrupt() {
        // U+0130 is 2 bytes but lowercases to 3, which used to shift every
        // index computed against a lowercased copy of the document.
        let html = "<html><head>\u{130}\u{130}\u{130}<base href=\"/x\"></head><body>OK</body></html>";
        let out = rewrite_html(html, "https://e.com/p");
        assert!(out.contains("</head>"), "{out}");
        assert!(out.contains("OK"), "{out}");
        assert!(out.contains("\u{130}\u{130}\u{130}"), "{out}");
        assert!(!out.contains(r#"href="/x""#), "{out}");
    }

    #[test]
    fn blocks_non_public_addresses() {
        for ip in [
            "127.0.0.1", "10.0.0.5", "192.168.1.1", "172.16.0.1",
            "169.254.169.254", "0.0.0.0", "100.64.0.1", "::1", "fe80::1",
            "fc00::1", "::ffff:127.0.0.1",
        ] {
            assert!(is_blocked_ip(&ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_blocked_ip(&ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn parses_host_and_port() {
        assert_eq!(split_host_port("https://e.com/a"), Some(("e.com".into(), 443)));
        assert_eq!(split_host_port("http://e.com/a"), Some(("e.com".into(), 80)));
        assert_eq!(split_host_port("http://e.com:8080/a?b=1"), Some(("e.com".into(), 8080)));
        assert_eq!(split_host_port("http://[::1]:9000/a"), Some(("::1".into(), 9000)));
        assert_eq!(split_host_port("http://[::1]/a"), Some(("::1".into(), 80)));
        // userinfo must not be mistaken for the host
        assert_eq!(split_host_port("http://user@127.0.0.1/a"), Some(("127.0.0.1".into(), 80)));
    }

    #[test]
    fn resolves_redirect_targets() {
        let b = "https://e.com/a/b";
        assert_eq!(resolve_redirect(b, "https://x.com/y"), "https://x.com/y");
        assert_eq!(resolve_redirect(b, "/c"), "https://e.com/c");
        assert_eq!(resolve_redirect(b, "//cdn.e.com/z"), "https://cdn.e.com/z");
        assert_eq!(resolve_redirect(b, "c"), "https://e.com/a/c");
    }

    #[test]
    fn noscript_is_unwrapped() {
        assert_eq!(unwrap_noscript("<noscript><img src=a></noscript>"), "<img src=a>");
    }
}
