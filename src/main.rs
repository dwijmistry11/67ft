use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use reqwest::Client;
use std::{sync::Arc, time::Duration};
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
}

/// Shared application state
pub struct AppState {
    pub client: Client,
    pub config: Config,
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
        .redirect(reqwest::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .expect("failed to build HTTP client");

    let state = Arc::new(AppState { client, config: config.clone() });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/health", get(health_handler))
        // Raw HTML endpoint (no link rewriting, just raw fetch)
        .route("/raw/*url", get(raw_handler))
        // Main proxy — catches /*url where url starts with http or https
        .route("/*url", get(proxy_handler))
        .with_state(state);

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
) -> Response {
    // URL comes in percent-encoded from the browser, decode it
    let target_url = decode_url(&url);

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
            error_page(&target_url, &e.to_string()).into_response()
        }
    }
}

/// Raw handler — returns the HTML without any link rewriting or UI injection
async fn raw_handler(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
) -> Response {
    let target_url = decode_url(&url);

    if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
        return (StatusCode::BAD_REQUEST, "URL must start with http:// or https://").into_response();
    }

    match proxy::fetch_and_rewrite(&state, &target_url, true).await {
        Ok(response) => response,
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// Decode a URL that may be percent-encoded, and reconstruct http:// or https:// prefix
fn decode_url(raw: &str) -> String {
    // The router gives us the path segment after the leading slash.
    // URLs might come in as:
    //   https%3A%2F%2Fexample.com  (percent-encoded by bookmarklet)
    //   https://example.com        (typed directly)
    let decoded = percent_decode(raw);

    // Axum strips the leading slash from the path wildcard,
    // but the scheme's // might have been collapsed. Normalise it.
    if decoded.starts_with("https:/") && !decoded.starts_with("https://") {
        decoded.replacen("https:/", "https://", 1)
    } else if decoded.starts_with("http:/") && !decoded.starts_with("http://") {
        decoded.replacen("http:/", "http://", 1)
    } else {
        decoded
    }
}

/// Minimal percent-decode (handles %XX sequences)
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
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
        url = url,
        reason = reason,
    ))
}
