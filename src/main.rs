use axum::{
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use reqwest::Client;
use std::{sync::Arc, time::Duration};
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tracing::info;

mod proxy;

/// 67ft — Self-hosted paywall bypass proxy
#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
pub struct Config {
    /// Port to listen on
    #[arg(short, long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Host/address to bind to
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// User-Agent string sent to upstream sites
    #[arg(
        short,
        long,
        env = "USER_AGENT",
        default_value = "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"
    )]
    pub user_agent: String,

    /// Spoof X-Forwarded-For IP (Googlebot IP by default)
    #[arg(long, env = "X_FORWARDED_FOR", default_value = "66.249.66.1")]
    pub forwarded_for: String,

    /// Request timeout in seconds
    #[arg(long, env = "TIMEOUT", default_value = "30")]
    pub timeout: u64,

    /// Maximum response size to buffer, in megabytes
    #[arg(long, env = "MAX_BODY_MB", default_value = "25")]
    pub max_body_mb: usize,

    /// How long to keep a fetched page in memory, in seconds. 0 disables the cache.
    #[arg(long, env = "CACHE_TTL", default_value = "300")]
    pub cache_ttl: u64,

    /// Memory budget for the page cache, in megabytes
    #[arg(long, env = "CACHE_MB", default_value = "32")]
    pub cache_mb: usize,

    /// Maximum number of requests handled at once. Further requests queue.
    #[arg(long, env = "MAX_CONCURRENT", default_value = "16")]
    pub max_concurrent: usize,

    /// Allow proxying to loopback, private and link-local addresses.
    /// Off by default: otherwise anyone who can reach this server can use it
    /// to probe the network it runs on.
    #[arg(long, env = "ALLOW_PRIVATE_HOSTS", default_value_t = false)]
    pub allow_private_hosts: bool,
}

/// Shared application state
pub struct AppState {
    pub client: Client,
    pub config: Config,
    pub cache: proxy::Cache,
}

// Embed the frontend at compile time — zero runtime filesystem reads
static INDEX_HTML: &str = include_str!("../static/index.html");

#[tokio::main]
async fn main() {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();

    // Build a persistent reqwest client (connection pool, etc.)
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout))
        // Redirects are followed manually in proxy::fetch_guarded so every hop
        // can be checked against the SSRF guard before it is requested.
        .redirect(reqwest::redirect::Policy::none())
        // Filter DNS answers at the point reqwest actually connects, so a name
        // cannot resolve to a public address for the check and a private one
        // for the request.
        .dns_resolver(Arc::new(proxy::GuardedResolver::new(
            config.allow_private_hosts,
        )))
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .expect("failed to build HTTP client");

    let cache = proxy::Cache::new(config.cache_ttl, config.cache_mb);
    let state = Arc::new(AppState { client, config: config.clone(), cache });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/health", get(health_handler))
        // Raw HTML endpoint (no link rewriting, just raw fetch)
        .route("/raw/*url", get(raw_handler))
        // Main proxy — catches /*url where url starts with http or https
        .route("/*url", get(proxy_handler))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                // Bound in-flight work: each request buffers a whole body, so
                // unbounded concurrency can exhaust a small host.
                .concurrency_limit(config.max_concurrent)
                // Article HTML compresses to a fraction of its size, which is
                // most of the transfer time over a home network.
                .layer(CompressionLayer::new()),
        );

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {addr}: {e}"));

    info!("🚀 67ft running on http://{addr}");
    axum::serve(listener, app).await.expect("server error");
}

/// Serve the landing page
async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Health check
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Proxy handler — fetches, rewrites, and serves the target page
async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let target_url = with_query(&decode_url(&url), query.as_deref());

    // Must look like a URL
    if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
        return (
            StatusCode::BAD_REQUEST,
            "URL must start with http:// or https://",
        )
            .into_response();
    }

    match proxy::fetch_and_rewrite(&state, &target_url, false).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("proxy error for {target_url}: {e}");
            (e.status(), error_page(&target_url, &e.to_string())).into_response()
        }
    }
}

/// Raw handler — returns the HTML without any link rewriting or UI injection
async fn raw_handler(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let target_url = with_query(&decode_url(&url), query.as_deref());

    if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
        return (StatusCode::BAD_REQUEST, "URL must start with http:// or https://").into_response();
    }

    match proxy::fetch_and_rewrite(&state, &target_url, true).await {
        Ok(response) => response,
        Err(e) => (e.status(), e.to_string()).into_response(),
    }
}

/// Normalise the target URL captured by the `/*url` wildcard.
///
/// Axum's `Path` extractor has already percent-decoded the segment, so the
/// value arrives ready to use. Decoding it a second time here corrupted any
/// URL containing a literal percent sign, turning `%2525` into `%` instead
/// of `%25`, and mangled non-ASCII bytes into Latin-1 characters.
fn decode_url(raw: &str) -> String {
    // Browsers and proxies sometimes collapse the "//" after the scheme.
    if raw.starts_with("https:/") && !raw.starts_with("https://") {
        raw.replacen("https:/", "https://", 1)
    } else if raw.starts_with("http:/") && !raw.starts_with("http://") {
        raw.replacen("http:/", "http://", 1)
    } else {
        raw.to_string()
    }
}

/// Append the request's own query string to the target URL.
///
/// The `/*url` wildcard captures only the path, so a typed URL such as
/// `/https://site/search?q=rust`, or a GET form submitted from a proxied page,
/// would otherwise lose everything after the `?`.
fn with_query(target: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() && !target.contains('?') => format!("{target}?{q}"),
        _ => target.to_string(),
    }
}

/// Escape text for safe interpolation into HTML.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Simple error page returned when proxying fails
fn error_page(url: &str, reason: &str) -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>67ft — Error</title>
<style>
body{{font-family:system-ui,sans-serif;background:#0d0d0d;color:#e8e8e8;display:flex;
flex-direction:column;align-items:center;justify-content:center;min-height:100vh;gap:1rem;padding:2rem}}
h1{{font-size:2rem;color:#f77}}
p{{color:#888;max-width:500px;text-align:center;word-break:break-all}}
a{{color:#7c6af7}}
</style></head><body>
<h1>Failed to fetch</h1>
<p>{reason}</p>
<p><a href="/">&larr; Try another URL</a></p>
<p style="font-size:0.75rem;color:#444">{url}</p>
</body></html>"#,
        url = escape_html(url),
        reason = escape_html(reason),
    ))
}
