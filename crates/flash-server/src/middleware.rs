//! Security middleware: response headers on everything, and a same-origin
//! requirement on state-changing web requests (token-less CSRF defense).

use std::collections::HashMap;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;

use crate::service::now_ms;
use crate::state::AppState;

/// Token-bucket rate limiter keyed by client IP, for auth-sensitive
/// endpoints. The address comes from `client_ip` below: a configured proxy
/// header, else the TCP peer, else (in tests) one shared bucket.
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, (f64, i64)>>,
    capacity: f64,
    per_minute: f64,
}

/// Bucket count past which idle buckets are evicted on the next call.
const EVICT_ABOVE: usize = 10_000;
/// Live-bucket count past which new keys are refused instead.
const HARD_CAP: usize = 50_000;

/// The limiter's key for a request: the route *template* (`/auth/reset/{token}`),
/// never the raw path, so a route with a parameter is one bucket per
/// client rather than one per value the client invents.
pub fn route_key(request: &Request) -> String {
    request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string())
}

impl RateLimiter {
    pub fn new(capacity: u32, per_minute: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity: capacity as f64,
            per_minute: per_minute as f64,
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let now = now_ms();
        let mut buckets = self.buckets.lock();
        if buckets.len() > EVICT_ABOVE {
            // Memory backstop. Idle buckets are the ones with nothing left
            // to remember: a bucket that has refilled completely behaves
            // exactly like a fresh one, so dropping it changes nothing.
            // Clearing everything would let whoever created the flood of
            // keys reset every other client's limit along with their own.
            let refill_ms = (self.capacity / self.per_minute * 60_000.0) as i64;
            buckets.retain(|_, (_, last)| now - *last < refill_ms);
            if buckets.len() > HARD_CAP {
                // Still flooded with live keys. The ones with most of
                // their budget left are not being throttled, so
                // forgetting them changes nothing for them and makes
                // room for whoever comes next; only the ones being
                // throttled are worth remembering.
                let half = self.capacity / 2.0;
                buckets.retain(|_, (tokens, _)| *tokens < half);
            }
            if buckets.len() > HARD_CAP && !buckets.contains_key(key) {
                // A flood of throttled keys: fail closed for newcomers
                // rather than forgetting the ones being throttled.
                return false;
            }
        }
        let (tokens, last) = buckets
            .entry(key.to_string())
            .or_insert((self.capacity, now));
        let refill = (now - *last) as f64 / 60_000.0 * self.per_minute;
        *tokens = (*tokens + refill).min(self.capacity);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The client's address for rate-limiting. A header is consulted only when
/// the operator named one (FLASH_CLIENT_IP_HEADER), because any header is
/// forgeable by whoever can reach the port directly; otherwise the TCP
/// peer address is used, and in tests, where there is no socket, every
/// request shares one bucket.
///
/// The header's value is parsed, never used verbatim: the rightmost
/// address of a comma-separated list is the one the trusted proxy
/// appended (the client controls the rest), an IPv6 address is keyed by
/// its /64 (one customer's whole allocation is one bucket), and a value
/// that is not an address at all falls back to the peer rather than
/// becoming a fresh bucket the client invented.
pub fn client_ip(request: &Request, trusted_header: Option<&str>) -> String {
    if let Some(name) = trusted_header {
        let parsed = request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(address_from_header);
        if let Some(key) = parsed {
            return key;
        }
    }
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| bucket_key(c.0.ip()))
        .unwrap_or_else(|| "local".to_string())
}

/// The bucket key for one address: IPv4 as is, IPv6 by its /64.
pub fn bucket_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// The address a proxy header names: the last entry of a
/// comma-separated list, parsed (a port suffix or brackets tolerated).
pub fn address_from_header(value: &str) -> Option<String> {
    let last = value.rsplit(',').next()?.trim();
    if last.is_empty() || last.len() > 64 {
        return None;
    }
    // `host:port` and `[v6]:port` first (the bracket form only parses
    // as a socket address), then a bare address with or without brackets.
    let ip = last
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|s| s.ip())
        .or_else(|| {
            last.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .ok()
        })?;
    Some(bucket_key(ip))
}

/// Applied to /oauth/* and /auth/* routes.
/// Every path segment and query string is bounded here, before routing:
/// a `Path<String>` or `RawQuery` a handler takes is at most
/// `bounds::URI` bytes by construction, so no handler has to say so.
/// hyper's own limit is the whole request head (hundreds of kilobytes);
/// a real URL of this application is a few hundred bytes.
pub async fn uri_length(req: Request, next: Next) -> Response {
    let uri = req.uri();
    let len = uri.path().len() + uri.query().map(str::len).unwrap_or(0);
    if len > crate::bounds::URI {
        return (StatusCode::URI_TOO_LONG, "the request URL is too long").into_response();
    }
    next.run(req).await
}

pub async fn auth_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = client_ip(&request, state.config.client_ip_header.as_deref());
    let key = format!("{ip}:{}", route_key(&request));
    if !state.rate_limiter.allow(&key) {
        return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
    }
    next.run(request).await
}

/// Applied to /api/v1 (all but its auth routes): the generous per-IP
/// budget, answered in the API's JSON envelope.
pub async fn api_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let key = format!(
        "api:{}",
        client_ip(&request, state.config.client_ip_header.as_deref())
    );
    if !state.api_rate_limiter.allow(&key) {
        return crate::api::ApiError::rate_limited().into_response();
    }
    next.run(request).await
}

/// Applied to /mcp: the API's per-IP budget, answered in plain text (the
/// JSON-RPC layer never sees a throttled request).
pub async fn mcp_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let key = format!(
        "mcp:{}",
        client_ip(&request, state.config.client_ip_header.as_deref())
    );
    if !state.api_rate_limiter.allow(&key) {
        return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
    }
    next.run(request).await
}

/// Applied to the browser band (site, web and extension pages): the
/// API's per-IP budget on every mutation, so an upload, an import, a
/// card save or a settings post cannot be repeated without limit from
/// one address. Reads pass untouched: pages, assets and htmx partials
/// are cheap and cacheable, and the heavy reads hold a permit.
pub async fn web_mutation_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let key = format!(
        "web:{}",
        client_ip(&request, state.config.client_ip_header.as_deref())
    );
    if !state.api_rate_limiter.allow(&key) {
        return (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
    }
    next.run(request).await
}

/// Applied to /api/v1/auth/*: the same strict IP+path budget as the web
/// auth routes, answered in the API's JSON envelope.
pub async fn api_auth_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let ip = client_ip(&request, state.config.client_ip_header.as_deref());
    let key = format!("{ip}:{}", route_key(&request));
    if !state.rate_limiter.allow(&key) {
        return crate::api::ApiError::rate_limited().into_response();
    }
    next.run(request).await
}

/// Paths whose responses are safe to cache: immutable assets and public,
/// content-addressed blobs. Everything else is either personal or
/// session-dependent and must not survive a logout in a shared browser.
fn cacheable_path(path: &str) -> bool {
    path.starts_with("/static/")
        || path.starts_with("/public/")
        || path.starts_with("/.well-known/")
        || path == "/favicon.ico"
        || path == "/robots.txt"
        || path == "/sitemap.xml"
        || (path.starts_with("/s/") && path.contains("/m/"))
}

pub async fn security_headers(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let cacheable = cacheable_path(request.uri().path());
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    // Handlers that know better (media with its own TTL, the API's
    // no-store) have already said so; the default for everything personal
    // is no-store, so the back button after "Sign out" shows nothing.
    if !cacheable && !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    // The page policy is built once at startup (`state::page_csp`).
    // Handlers may pre-set a route-specific CSP (the OAuth consent page
    // must); the default only fills in when none is present.
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        if let Ok(value) = HeaderValue::from_str(&state.csp) {
            h.insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // same-origin (not no-referrer): Firefox suppresses the Origin header
    // to "null" on form POSTs under no-referrer, which would trip the
    // same-origin mutation guard. Nothing leaks cross-site either way.
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    h.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response
}

/// Rejects cross-origin mutations. Browsers send `Origin` on all
/// non-GET requests; htmx requests are same-origin by construction.
/// Applied to web routes only — OAuth/MCP endpoints authenticate with
/// Bearer tokens and are mounted outside this layer.
pub async fn same_origin_mutations(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe {
        let headers = request.headers();
        let origin = headers.get(header::ORIGIN);
        let fetch_site = headers.get("sec-fetch-site");
        let origin_ok = match origin.map(|o| o.as_bytes()) {
            // A present, real Origin must match exactly.
            Some(o) if o != b"null" => o == state.config.base_url.as_bytes(),
            // Origin absent or "null" (privacy modes, some Firefox
            // configurations): fall back to the Sec-Fetch-Site signal.
            _ => fetch_site
                .map(|v| v.as_bytes() == b"same-origin")
                .unwrap_or(false),
        };
        if !origin_ok {
            tracing::warn!(
                path = ?request.uri().path(),
                origin = ?origin,
                sec_fetch_site = ?fetch_site,
                "cross-origin mutation rejected"
            );
            return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
        }
    }
    next.run(request).await
}

/// What a request whose handler panicked gets back: a plain 500 with
/// the panic's message in the log and nothing about it in the reply.
/// Installed by `build_app` as the `CatchPanicLayer` handler.
pub fn panic_response(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let message = if let Some(s) = err.downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        s
    } else {
        "non-string panic payload"
    };
    tracing::error!(panic = ?message, "request handler panicked");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flood_of_new_keys_does_not_reset_a_throttled_one() {
        let limiter = RateLimiter::new(3, 1);
        for _ in 0..3 {
            assert!(limiter.allow("attacker:/login"));
        }
        assert!(!limiter.allow("attacker:/login"), "budget spent");
        // Inventing keys past the eviction threshold used to clear the
        // whole map, budget included.
        for i in 0..(EVICT_ABOVE + 5) {
            limiter.allow(&format!("attacker:/enroll/{i}"));
        }
        assert!(
            !limiter.allow("attacker:/login"),
            "the throttled bucket survived the flood"
        );
    }

    #[test]
    fn idle_buckets_are_evicted_and_live_ones_kept() {
        let limiter = RateLimiter::new(2, 1_000_000);
        // Fill past the threshold with buckets that refill instantly, so
        // they count as idle on the next eviction pass.
        for i in 0..(EVICT_ABOVE + 1) {
            limiter.allow(&format!("k{i}"));
        }
        assert!(limiter.allow("newcomer"), "idle buckets made room");
        assert!(limiter.buckets.lock().len() < EVICT_ABOVE);
    }

    /// The proxy header is parsed, not trusted verbatim: the client
    /// controls everything but the rightmost entry, garbage is not a
    /// key, and an IPv6 customer is one bucket.
    #[test]
    fn a_proxy_header_yields_the_appended_address_or_nothing() {
        assert_eq!(
            address_from_header("203.0.113.9").as_deref(),
            Some("203.0.113.9")
        );
        // The client sent "1.2.3.4, 5.6.7.8" and the proxy appended itself's view.
        assert_eq!(
            address_from_header("1.2.3.4, 5.6.7.8, 203.0.113.9").as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            address_from_header("[2001:db8:1:2:3:4:5:6]:443").as_deref(),
            Some("2001:db8:1:2::/64")
        );
        assert_eq!(
            address_from_header("2001:db8:1:2:ffff::1").as_deref(),
            Some("2001:db8:1:2::/64"),
            "one /64 is one bucket"
        );
        for garbage in ["", "not-an-ip", &"x".repeat(100), "1.2.3.4, "] {
            assert_eq!(address_from_header(garbage), None, "{garbage:?}");
        }
    }
}
