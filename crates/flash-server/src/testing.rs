//! Shared scaffolding for the HTTP integration tests: one test `Config`,
//! one way to build the router with fakes swapped in, one way to mint
//! sessions and tokens against the store, and one way to send a request
//! and read the reply. Behind the `test-support` feature so this crate's
//! tests and a downstream crate's tests build the app the same way.

pub mod routes;

use std::path::PathBuf;
use std::sync::Arc;

use crate::api::{MOBILE_CLIENT_ID, MOBILE_SCOPE};
use crate::auth::{hash_token, new_token, SESSION_TTL_MS};
use crate::config::Config;
use crate::email::Mailer;
use crate::ext::ServerExtension;
use crate::media_store::{Blob, ByteRange, DiskStore, MediaStore, MediaStoreError};
use crate::policy::{CardPolicy, Unlimited};
use crate::service::{now_ms, Services};
use crate::state::AppState;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use flash_core::UserId;
use flash_store::Store;
use tower::ServiceExt;

/// The canonical origin every test config uses; same-origin form posts
/// must send it as `Origin`.
pub const BASE: &str = "http://localhost:8437";

/// A fresh per-binary, per-tag scratch directory (media, import previews).
pub fn data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("flash-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The all-features-off config.
pub fn config(data_dir: PathBuf) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        client_ip_header: None,
        base_url: BASE.to_string(),
        support_email: None,
        data_dir,
        mail: None,
        dev_mail_log: false,
        media_r2: None,
    }
}

/// A built router plus the handles tests reach past it with.
pub struct TestApp {
    pub app: Router,
    pub store: Arc<Store>,
    pub services: Services,
    pub data_dir: PathBuf,
}

/// Builds an `AppState` the way production does, then swaps in whichever
/// fakes the test provides — exactly the overrides the per-file harnesses
/// used to apply by hand.
pub struct AppBuilder {
    tag: String,
    store: Option<Arc<Store>>,
    mailer: Option<Arc<dyn Mailer>>,
    ext: Option<Arc<dyn ServerExtension>>,
    policy: Option<Arc<dyn CardPolicy>>,
    media: Option<Arc<dyn MediaStore>>,
    tight_limits: bool,
}

/// The burst every limiter allows under `tight_rate_limits`: a probe
/// sends one more than this and expects 429.
pub const TIGHT_BURST: u32 = 3;
/// The generous band's burst under `tight_rate_limits`: two more than
/// the strict band's, so the request that draws the 429 says which
/// limiter a route sits behind.
pub const TIGHT_BURST_GENEROUS: u32 = TIGHT_BURST + 2;

impl AppBuilder {
    pub fn new(tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            store: None,
            mailer: None,
            ext: None,
            policy: None,
            media: None,
            tight_limits: false,
        }
    }

    /// Every rate limiter allows `TIGHT_BURST` requests and refills
    /// slowly, so a test can see a 429 in a handful of requests. In the
    /// harness there is no socket, so every request shares one bucket.
    pub fn tight_rate_limits(mut self) -> Self {
        self.tight_limits = true;
        self
    }

    /// The media store to install instead of the disk store under the
    /// test's data dir: `PresigningStore` to exercise the redirect path.
    pub fn media(mut self, media: Arc<dyn MediaStore>) -> Self {
        self.media = Some(media);
        self
    }

    /// The server extension to install; the hosted product's unless a
    /// test asks for the open server's `CoreOnly`.
    pub fn ext(mut self, ext: Arc<dyn ServerExtension>) -> Self {
        self.ext = Some(ext);
        self
    }

    /// The card policy `Services` consults; `Unlimited` unless the hosted
    /// product's harness plugs its cap in.
    pub fn policy(mut self, policy: Arc<dyn CardPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Builds over an existing store (rebuilding the router mid-test with
    /// different fakes) instead of a fresh in-memory one.
    pub fn store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Present => the mail loops (password reset; signup on the hosted
    /// product) are enabled with this capture mailer.
    pub fn mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mailer = Some(mailer);
        self
    }

    pub fn build(self) -> TestApp {
        let store = self
            .store
            .unwrap_or_else(|| Arc::new(Store::open_in_memory().unwrap()));
        let policy = self.policy.unwrap_or_else(|| Arc::new(Unlimited));
        let services = Services::with_policy(store.clone(), policy);
        let data_dir = data_dir(&self.tag);
        let config = config(data_dir.clone());
        let ext = self.ext.unwrap_or_else(|| Arc::new(crate::ext::CoreOnly));
        let mut state = AppState::new(services.clone(), config, ext, now_ms()).unwrap();
        if let Some(mailer) = self.mailer {
            state.mailer = Some(mailer);
        }
        if let Some(media) = self.media {
            state.install_media(media);
        }
        if self.tight_limits {
            // Two different bursts, so a probe can tell which band
            // answered: the strict one (sign-in, reset, token minting)
            // refuses first.
            state.rate_limiter = Arc::new(crate::middleware::RateLimiter::new(TIGHT_BURST, 1));
            state.api_rate_limiter =
                Arc::new(crate::middleware::RateLimiter::new(TIGHT_BURST_GENEROUS, 1));
        }
        TestApp {
            app: crate::build_app(state),
            store,
            services,
            data_dir,
        }
    }
}

// ---- fixtures ----

/// A heavy-job permit for `user` from a gate of its own, for tests that
/// call an import, export or Stats function directly.
pub fn heavy_permit(user: UserId) -> crate::state::HeavyPermit {
    Arc::new(crate::state::HeavyJobs::new(1))
        .acquire(user)
        .expect("fresh gate")
}

/// A member account.
pub fn member(store: &Store, name: &str, email: &str) -> UserId {
    store
        .create_user(name, Some(email), "member", now_ms())
        .unwrap()
}

/// Mints a web session for `user` directly against the store and returns
/// the cookie header value a browser would send back.
pub fn web_session(store: &Store, user: UserId) -> String {
    let token = new_token();
    store
        .create_web_session(
            &hash_token(&token),
            user,
            now_ms(),
            now_ms() + SESSION_TTL_MS,
        )
        .unwrap();
    format!("__Host-flash={token}")
}

/// A verified member plus a live session cookie.
pub fn signed_in(store: &Store, name: &str, email: &str) -> (UserId, String) {
    let user = member(store, name, email);
    let cookie = web_session(store, user);
    (user, cookie)
}

/// An OAuth access token for `user` (the MCP bearer), minted directly.
pub fn oauth_bearer(store: &Store, user: UserId) -> String {
    let now = now_ms();
    let access = new_token();
    store
        .insert_oauth_token(
            &hash_token(&access),
            &hash_token(&new_token()),
            "test-client",
            user,
            "flashcards",
            "",
            now + 3_600_000,
            now + 86_400_000,
            now,
        )
        .unwrap();
    access
}

/// A first-party (mobile app) bearer for `user`, minted directly the way
/// the API's grant flow does.
pub fn api_bearer(store: &Store, user: UserId) -> String {
    let now = now_ms();
    let access = new_token();
    store
        .insert_oauth_token(
            &hash_token(&access),
            &hash_token(&new_token()),
            MOBILE_CLIENT_ID,
            user,
            MOBILE_SCOPE,
            "test device",
            now + 3_600_000,
            now + 86_400_000,
            now,
        )
        .unwrap();
    access
}

// ---- requests ----

/// A JSON API request: optional bearer, optional body.
pub fn json_req(
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ACCEPT, "application/json");
    if let Some(b) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    match body {
        Some(json) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    }
}

pub fn json_get(path: &str, bearer: Option<&str>) -> Request<Body> {
    json_req("GET", path, bearer, None)
}

pub fn json_post(path: &str, bearer: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    json_req("POST", path, bearer, Some(body))
}

pub fn json_put(path: &str, bearer: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    json_req("PUT", path, bearer, Some(body))
}

pub fn json_patch(path: &str, bearer: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    json_req("PATCH", path, bearer, Some(body))
}

pub fn json_delete(path: &str, bearer: Option<&str>) -> Request<Body> {
    json_req("DELETE", path, bearer, None)
}

/// A fully read response.
pub struct Reply {
    pub status: StatusCode,
    pub headers: Vec<(String, String)>,
    pub bytes: Vec<u8>,
}

impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.bytes).unwrap_or(serde_json::Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    /// The Location header, or "" when absent.
    pub fn location(&self) -> &str {
        self.header("location").unwrap_or("")
    }

    /// Every Set-Cookie header value.
    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Runs one request through the router and reads the whole reply.
pub async fn send(app: &Router, req: Request<Body>) -> Reply {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

/// Case-insensitive header lookup in a captured header list.
pub fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A GET, optionally signed in. No User-Agent: the public share pages
/// treat UA-less requests as bots, and tests that care set one.
pub fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut req = Request::get(path);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::empty()).unwrap()
}

/// A same-origin urlencoded form post, optionally signed in.
pub fn form_post(path: &str, cookie: Option<&str>, body: &str) -> Request<Body> {
    let mut req = Request::post(path)
        .header(header::ORIGIN, BASE)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    req.body(Body::from(body.to_string())).unwrap()
}

// ---- media store fakes ----

/// The origin `PresigningStore` hands out URLs on.
pub const PRESIGN_ORIGIN: &str = "https://media.example";

/// A disk store that also offers presigned URLs, the way the bucket
/// store does in production: `https://media.example/<key>?sig=test`.
/// Bytes still come from disk for uploads, exports and the API's byte
/// route.
pub struct PresigningStore {
    disk: DiskStore,
}

impl PresigningStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            disk: DiskStore::new(root),
        }
    }
}

impl MediaStore for PresigningStore {
    fn put(&self, sha256: &str, mime: &str, bytes: &[u8]) -> Result<(), MediaStoreError> {
        self.disk.put(sha256, mime, bytes)
    }

    fn get(&self, sha256: &str, range: Option<ByteRange>) -> Result<Blob, MediaStoreError> {
        self.disk.get(sha256, range)
    }

    fn delete(&self, sha256: &str) -> Result<(), MediaStoreError> {
        self.disk.delete(sha256)
    }

    fn describe(&self) -> String {
        "presigning test store".into()
    }

    fn presigned_get(
        &self,
        sha256: &str,
        mime: &str,
        _filename: &str,
        ttl: std::time::Duration,
    ) -> Option<Result<String, MediaStoreError>> {
        Some(Ok(format!(
            "{PRESIGN_ORIGIN}/{sha256}?sig=test&mime={mime}&ttl={}",
            ttl.as_secs()
        )))
    }

    fn origin(&self) -> Option<String> {
        Some(PRESIGN_ORIGIN.to_string())
    }
}
