use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::debug;

use crate::AppState;

/// Response headers we REMOVE from upstream before sending to client.
/// These would block rendering or track the user.
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
    "set-cookie",      // don't leak site cookies to our domain
    "link",            // can contain preload hints that break things
];

/// Fetch a URL as Googlebot, optionally rewrite links so navigation stays proxied.
/// If `raw` is true, returns the HTML unmodified (no link rewriting).
pub async fn fetch_and_rewrite(
    state: &Arc<AppState>,
    url: &str,
    raw: bool,
) -> Result<Response, FetchError> {
    debug!("fetching: {url}");

    let response = state
        .client
        .get(url)
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

    let status = response.status();
    let upstream_headers = response.headers().clone();

    // Determine content-type
    let content_type = upstream_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();

    // For non-HTML content (images, CSS, JS, fonts etc.), just stream it through as-is.
    // This handles when links point to the same domain and get routed through the proxy.
    let is_html = content_type.contains("text/html");

    if !is_html || raw {
        // Stream binary / non-HTML content directly
        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| FetchError::Body(e.to_string()))?;

        let mut builder = Response::builder().status(status_convert(status));

        // Forward safe headers
        let headers = builder.headers_mut().unwrap();
        forward_headers(&upstream_headers, headers);

        // Ensure content-type is set
        headers.insert(
            "content-type",
            HeaderValue::from_str(&content_type).unwrap_or_else(|_| {
                HeaderValue::from_static("application/octet-stream")
            }),
        );

        return Ok(builder
            .body(Body::from(body_bytes))
            .unwrap()
            .into_response());
    }

    // --- HTML path ---
    let html_bytes = response
        .bytes()
        .await
        .map_err(|e| FetchError::Body(e.to_string()))?;

    let html = String::from_utf8_lossy(&html_bytes).into_owned();
    let rewritten = rewrite_html(&html, url);

    let mut builder = Response::builder()
        .status(status_convert(status))
        .header("content-type", "text/html; charset=utf-8");

    // Strip tracking/security headers, forward safe ones
    let headers = builder.headers_mut().unwrap();
    forward_headers(&upstream_headers, headers);

    Ok(builder
        .body(Body::from(rewritten))
        .unwrap()
        .into_response())
}

/// Rewrite HTML so that:
/// 1. A <base href="origin"> is injected — lets browser resolve relative
///    resource URLs (CSS, images, scripts) natively without proxying them.
/// 2. All <a href> links pointing to the same origin are rewritten to go
///    through 67ft so navigation remains proxied.
fn rewrite_html(html: &str, original_url: &str) -> String {
    let origin = extract_origin(original_url);

    // Step 1: Remove any existing <base> tag (sites sometimes set base href)
    let html = remove_existing_base(html);

    // Step 2: Inject our <base href> right after <head> (or at start if no head)
    let base_tag = format!(r#"<base href="{origin}/">"#);
    let html = inject_after_head(&html, &base_tag);

    // Step 3: Rewrite <a href="..."> links that point to the same origin
    // so they continue to be served through the proxy.
    let html = rewrite_anchor_links(&html, &origin);

    // Step 4: Remove common paywall script patterns
    // (heuristic — targets known paywall JS variable names)
    let html = remove_paywall_hints(&html);

    html
}

/// Extract scheme + host from a URL, e.g. "https://www.nytimes.com"
fn extract_origin(url: &str) -> String {
    // Find the end of the scheme (after "://")
    if let Some(after_scheme) = url.find("://") {
        let rest = &url[after_scheme + 3..];
        // Take up to the first '/'
        let host_end = rest.find('/').unwrap_or(rest.len());
        let host = &rest[..host_end];
        let scheme = &url[..after_scheme];
        return format!("{scheme}://{host}");
    }
    // Fallback
    url.to_string()
}

/// Remove existing <base ...> tags
fn remove_existing_base(html: &str) -> String {
    // Simple case-insensitive removal
    let lower = html.to_lowercase();
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;

    while let Some(start) = lower[pos..].find("<base") {
        let abs_start = pos + start;
        // Find the end of this tag
        let tag_rest = &lower[abs_start..];
        let end_offset = tag_rest.find('>').map(|e| e + 1).unwrap_or(tag_rest.len());
        // Copy everything before the tag
        result.push_str(&html[pos..abs_start]);
        // Skip the tag
        pos = abs_start + end_offset;
    }
    result.push_str(&html[pos..]);
    result
}

/// Inject a string immediately after the opening <head> tag (or prepend to <html>)
fn inject_after_head(html: &str, injection: &str) -> String {
    let lower = html.to_lowercase();
    if let Some(head_pos) = lower.find("<head") {
        // Find the end of the opening <head ...> tag
        if let Some(close) = lower[head_pos..].find('>') {
            let insert_at = head_pos + close + 1;
            let mut out = String::with_capacity(html.len() + injection.len() + 2);
            out.push_str(&html[..insert_at]);
            out.push('\n');
            out.push_str(injection);
            out.push_str(&html[insert_at..]);
            return out;
        }
    }
    // No <head> found — prepend
    format!("{injection}\n{html}")
}

/// Rewrite <a href="..."> links so same-origin navigation goes through 67ft.
/// We rewrite both absolute same-origin URLs and root-relative paths (/path/...).
fn rewrite_anchor_links(html: &str, origin: &str) -> String {
    let mut out = String::with_capacity(html.len() + 512);
    // We walk through the HTML looking for href=" patterns inside <a tags.
    // This is a conservative string scan — not a full parser, but fast and small.

    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Look for href=
        if i + 6 < len && bytes[i..i + 5].eq_ignore_ascii_case(b"href=") {
            out.push_str(&html[..i]); // push everything up to here via drain trick — we need ref slicing
            // We reconstruct by pushing the prefix each iteration, reset slice
            // Actually let's use index-based push approach properly:
            // (the above push_str is wrong because we'll double-push — rewrite properly below)
            out.clear();
            break;
        }
        i += 1;
    }

    // Simpler correct approach: scan for href="..." and href='...' patterns
    rewrite_hrefs(html, origin)
}

fn rewrite_hrefs(html: &str, origin: &str) -> String {
    let mut out = String::with_capacity(html.len() + 512);
    let mut rest = html;

    while !rest.is_empty() {
        // Find the next href= (case-insensitive search)
        let lower_rest = rest.to_lowercase();
        let href_pos = match lower_rest.find("href=") {
            Some(p) => p,
            None => {
                out.push_str(rest);
                break;
            }
        };

        // Push everything before "href="
        out.push_str(&rest[..href_pos]);
        let after_href = &rest[href_pos + 5..]; // skip "href="

        // Determine quote character
        let (quote_char, url_start) = if after_href.starts_with('"') {
            ('"', &after_href[1..])
        } else if after_href.starts_with('\'') {
            ('\'', &after_href[1..])
        } else {
            // No quotes — just emit as-is
            out.push_str("href=");
            rest = after_href;
            continue;
        };

        // Find end of URL
        let url_end = url_start.find(quote_char).unwrap_or(url_start.len());
        let href_val = &url_start[..url_end];

        // Decide whether to rewrite
        let new_href = rewrite_single_href(href_val, origin);

        out.push_str("href=");
        out.push(quote_char);
        out.push_str(&new_href);
        out.push(quote_char);

        // Advance past the closing quote
        rest = &url_start[url_end + 1..];
    }

    out
}

/// Given a single href value, rewrite it to go through the proxy if it's a navigable link.
fn rewrite_single_href(href: &str, origin: &str) -> String {
    // Skip: fragment-only, mailto, javascript, tel, data, empty
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

    // Absolute same-origin URL
    if href.starts_with(origin) {
        return format!("/{}", encode_url(href));
    }

    // Absolute URL to a different domain — let it go through proxy too
    // (this keeps multi-domain article sites working)
    if href.starts_with("http://") || href.starts_with("https://") {
        return format!("/{}", encode_url(href));
    }

    // Root-relative path like /article/foo
    if href.starts_with('/') {
        let full = format!("{origin}{href}");
        return format!("/{}", encode_url(&full));
    }

    // Relative path — the <base> tag handles these; don't rewrite
    href.to_string()
}

/// Percent-encode a URL for embedding in a path segment.
/// We only encode characters that would confuse path parsing.
fn encode_url(url: &str) -> String {
    // We want the URL to be the path segment directly after /.
    // Browsers will send it decoded, so minimal encoding needed.
    // Just make sure it doesn't contain unencoded spaces.
    url.replace(' ', "%20")
}

/// Remove common paywall hint patterns.
/// This is heuristic and best-effort.
fn remove_paywall_hints(html: &str) -> String {
    // Many paywalls check window.__PIANO_ID__, window.tp, etc. in inline scripts.
    // Rather than trying to parse and remove scripts (fragile), we override their
    // gate-keeping variables with a simple injection into the page.
    // We inject a script that preemptively neutralises common paywall globals.
    let neutraliser = r#"<script>
/* 67ft paywall neutraliser */
try {
  // Piano SDK (used by many publishers)
  window.tp = window.tp || [];
  if (Array.isArray(window.tp)) {
    window.tp.push(["setUseTinypassAccounts", false]);
    window.tp.push(["setAid", ""]);
  }
  // Prevent metered paywall counters
  try { window.localStorage.removeItem("_pc"); } catch(e) {}
  // NYT, Atlantic etc.
  window.__tnt = window.__tnt || {};
  window.__tnt.user = window.__tnt.user || { loggedIn: true, subscriptionActive: true };
  // Medium
  window.__APOLLO_STATE__ = window.__APOLLO_STATE__ || {};
} catch(e) {}
</script>"#;

    // Inject just before </head>
    let lower = html.to_lowercase();
    if let Some(pos) = lower.find("</head>") {
        let mut out = String::with_capacity(html.len() + neutraliser.len());
        out.push_str(&html[..pos]);
        out.push_str(neutraliser);
        out.push('\n');
        out.push_str(&html[pos..]);
        out
    } else {
        html.to_string()
    }
}

/// Copy safe response headers from upstream to our response.
fn forward_headers(upstream: &HeaderMap, dest: &mut HeaderMap) {
    for (name, value) in upstream.iter() {
        let name_str = name.as_str();
        // Skip headers we strip
        if STRIP_RESPONSE_HEADERS.contains(&name_str) {
            continue;
        }
        // Skip hop-by-hop headers
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
        // Insert; ignore errors (e.g. non-ASCII header values)
        if let Ok(header_name) = HeaderName::try_from(name_str) {
            dest.insert(header_name, value.clone());
        }
    }
}

/// Convert reqwest StatusCode → axum StatusCode
fn status_convert(s: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(s.as_u16()).unwrap_or(StatusCode::OK)
}

/// Errors that can occur during proxying
#[derive(Debug)]
pub enum FetchError {
    Request(String),
    Body(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Request(e) => write!(f, "request failed: {e}"),
            FetchError::Body(e) => write!(f, "reading response body failed: {e}"),
        }
    }
}
