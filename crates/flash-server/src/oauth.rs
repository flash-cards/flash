//! OAuth 2.1 authorization server + resource server, hand-rolled.
//! Public clients only (PKCE S256 mandatory), dynamic client registration
//! (RFC 7591), server metadata (RFC 8414), protected-resource metadata
//! (RFC 9728). Tokens are opaque 256-bit randoms stored as SHA-256.

use askama::Template;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use flash_core::UserId;
use flash_store::CodeConsume;
use serde::Deserialize;

use crate::auth::{hash_token, new_token, pkce_s256, AuthUser};
use crate::ext::Surface;
use crate::service::now_ms;
use crate::state::{AppState, Ceremony};

const CODE_TTL_MS: i64 = 60 * 1000;
/// Token lifetimes, shared with the app's first-party grant flow.
pub(crate) const ACCESS_TTL_MS: i64 = 60 * 60 * 1000;
pub(crate) const REFRESH_TTL_MS: i64 = 60 * 24 * 60 * 60 * 1000;
const CONSENT_TTL_MS: i64 = 10 * 60 * 1000;
const DEFAULT_SCOPE: &str = "flashcards";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        // MCP clients may request the path-suffixed form for a resource
        // living at /mcp (RFC 9728 section 3).
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/oauth/register",
            post(register).layer(axum::extract::DefaultBodyLimit::max(REGISTER_BODY_LIMIT)),
        )
        .route("/oauth/token", post(token))
}

/// The two endpoints a browser drives: the authorization request, which
/// renders the consent page to the signed-in user, and the consent form
/// itself. They mount in the browser band, so the consent POST sits
/// behind the same-origin guard as well as its single-use ceremony token.
pub fn browser_router() -> Router<AppState> {
    Router::new()
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/consent", post(consent))
}

// ---- metadata ----

async fn authorization_server_metadata(State(state): State<AppState>) -> Response {
    let base = &state.config.base_url;
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [DEFAULT_SCOPE],
    }))
    .into_response()
}

async fn protected_resource_metadata(State(state): State<AppState>) -> Response {
    let base = &state.config.base_url;
    Json(serde_json::json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [DEFAULT_SCOPE],
    }))
    .into_response()
}

// ---- dynamic client registration ----

/// The two fields this server acts on. Anything else a client sends
/// (RFC 7591 allows a lot) is accepted and dropped: this endpoint is
/// unauthenticated, so nothing it receives may be stored unbounded.
#[derive(Deserialize)]
struct RegisterRequest {
    #[serde(deserialize_with = "crate::bounds::redirect_uris")]
    redirect_uris: Vec<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_client_name")]
    client_name: Option<String>,
}

/// More than enough for ten redirect URIs and a name; the endpoint is
/// unauthenticated, so the default 2 MiB body limit was a write amplifier.
const REGISTER_BODY_LIMIT: usize = 16 * 1024;

/// A redirect URI a client may register: an absolute `https` URL, or
/// plain `http` to a loopback *host* (local tooling, RFC 8252 §7.3).
/// Parsed as a URL, not matched as a prefix: `http://localhost@evil`
/// has host `evil`, `http://localhost.evil` is not loopback, and a
/// fragment or userinfo has no place in a redirect target.
fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if uri.chars().any(|c| c.is_control()) {
        return false;
    }
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => matches!(
            url.host(),
            Some(url::Host::Domain("localhost"))
                | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
                | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
        ),
        _ => false,
    }
}

/// The page an authorize request gets when it is malformed after the
/// client and redirect URI checked out. Never a redirect: anyone can
/// register a client with any `https` URI, so bouncing an error to the
/// registered URI without a click would make the authorize endpoint an
/// open redirector from this origin. The redirect happens only after
/// the person presses Approve or Deny.
#[derive(Template)]
#[template(path = "oauth_error.html")]
struct OAuthErrorPage {
    /// The OAuth error code, for the person to quote back to the app.
    code: &'static str,
    theme: &'static str,
}

fn authorize_error(code: &'static str, theme: crate::auth::Theme) -> Response {
    let page = OAuthErrorPage {
        code,
        theme: theme.as_str(),
    };
    (
        StatusCode::BAD_REQUEST,
        Html(page.render().unwrap_or_default()),
    )
        .into_response()
}

async fn register(State(state): State<AppState>, Json(req): Json<RegisterRequest>) -> Response {
    if req.redirect_uris.is_empty() || req.redirect_uris.len() > 10 {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "1-10 redirect_uris required",
        );
    }
    for uri in &req.redirect_uris {
        if !valid_redirect_uri(uri) || uri.len() > 500 {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "redirect_uris must be https:// (or loopback http)",
            );
        }
    }
    let client_name = req
        .client_name
        .as_deref()
        .unwrap_or("Unnamed client")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect::<String>();
    let client_name = if client_name.trim().is_empty() {
        "Unnamed client".to_string()
    } else {
        client_name
    };

    let client_id = new_token();
    let now = now_ms();
    let metadata = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": req.redirect_uris,
    })
    .to_string();
    let services = state.services.clone();
    let redirect_uris = req.redirect_uris.clone();
    let stored_id = client_id.clone();
    let stored_name = client_name.clone();
    let result = tokio::task::spawn_blocking(move || {
        services.store().create_oauth_client(
            &stored_id,
            &stored_name,
            &redirect_uris,
            &metadata,
            now,
        )
    })
    .await;
    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "client_id": client_id,
                "client_name": client_name,
                "redirect_uris": req.redirect_uris,
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "client_id_issued_at": now / 1000,
            })),
        )
            .into_response(),
        other => {
            tracing::error!("client registration failed: {other:?}");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "registration failed",
            )
        }
    }
}

// ---- authorize + consent ----

#[derive(Deserialize, Clone)]
pub struct AuthorizeParams {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    pub response_type: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    pub client_id: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_redirect_uri")]
    pub redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_oauth_state")]
    pub state: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_code_challenge")]
    pub code_challenge: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    pub code_challenge_method: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_oauth_scope")]
    pub scope: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_oauth_resource")]
    pub resource: Option<String>,
}

/// Fully-validated authorize request, held server-side while the user
/// looks at the consent screen.
#[derive(Clone)]
pub struct PendingConsent {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub scope: String,
    pub resource: Option<String>,
    pub user: UserId,
}

#[derive(Template)]
#[template(path = "consent.html")]
struct ConsentPage {
    client_name: String,
    /// Where the authorization code will be sent. Anyone can register a
    /// client under any name, so the name alone is not something to
    /// approve on; the destination host is.
    redirect_host: String,
    token: String,
    theme: &'static str,
}

/// `host[:port]` of an absolute URL, for display. The URI was validated
/// at registration and matched exactly here, so this is presentation.
fn display_host(uri: &str) -> String {
    let after_scheme = uri.split_once("://").map(|(_, r)| r).unwrap_or(uri);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_string()
}

async fn authorize(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    theme: crate::auth::Theme,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    // Client + redirect must validate before anything is redirected.
    let Some(client_id) = params.client_id.clone() else {
        return (StatusCode::BAD_REQUEST, "missing client_id").into_response();
    };
    let services = state.services.clone();
    let lookup_id = client_id.clone();
    let client =
        match tokio::task::spawn_blocking(move || services.store().get_oauth_client(&lookup_id))
            .await
        {
            Ok(Ok(Some(client))) => client,
            Ok(Ok(None)) => return (StatusCode::BAD_REQUEST, "unknown client_id").into_response(),
            other => {
                tracing::error!("client lookup: {other:?}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        };
    let Some(redirect_uri) = params.redirect_uri.clone() else {
        return (StatusCode::BAD_REQUEST, "missing redirect_uri").into_response();
    };
    if !client.redirect_uris.iter().any(|u| u == &redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not registered").into_response();
    }

    // A malformed request is a page, never a redirect: the registered
    // URI is the client's own choice, and the person has not clicked
    // anything yet (see `authorize_error`).
    if params.response_type.as_deref() != Some("code") {
        return authorize_error("unsupported_response_type", theme);
    }
    let Some(code_challenge) = params.code_challenge.clone() else {
        return authorize_error("invalid_request", theme);
    };
    if params.code_challenge_method.as_deref() != Some("S256") {
        return authorize_error("invalid_request", theme);
    }
    // RFC 8707: a resource indicator, when given, must be this server's
    // MCP endpoint; a token for somewhere else is not something to mint.
    if let Some(resource) = params.resource.as_deref() {
        let ours = format!("{}/mcp", state.config.base_url);
        if resource.trim_end_matches('/') != ours {
            return authorize_error("invalid_target", theme);
        }
    }
    if params
        .scope
        .as_deref()
        .is_some_and(|s| s.split_whitespace().any(|w| w != DEFAULT_SCOPE))
    {
        return authorize_error("invalid_scope", theme);
    }

    let consent_token = new_token();
    let redirect_host = display_host(&redirect_uri);
    state.put_ceremony(
        hash_token(&consent_token),
        Ceremony::Consent {
            pending: PendingConsent {
                client_id,
                redirect_uri,
                state: params.state.clone(),
                code_challenge,
                scope: params.scope.unwrap_or_else(|| DEFAULT_SCOPE.into()),
                resource: params.resource.clone(),
                user,
            },
            expires_ms: now_ms() + CONSENT_TTL_MS,
        },
    );
    let page = ConsentPage {
        client_name: client.client_name,
        redirect_host,
        token: consent_token,
        theme: theme.as_str(),
    };
    let mut response = Html(page.render().unwrap_or_default()).into_response();
    // Chrome enforces form-action against the POST's *redirect target*,
    // and approving consent redirects to the client's callback: https for
    // the hosted connectors, plain-http loopback for desktop clients such
    // as Claude Code and Claude Desktop (RFC 8252 §7.3). The redirect URI
    // itself is validated against the client's registration server-side;
    // the sources here only unblock the redirect.
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'none'; base-uri 'none'; \
             form-action 'self' https: http://localhost:* http://127.0.0.1:*",
        ),
    );
    response
}

#[derive(Deserialize)]
struct ConsentForm {
    #[serde(deserialize_with = "crate::bounds::token")]
    token: String,
    #[serde(deserialize_with = "crate::bounds::keyword")]
    decision: String,
}

async fn consent(
    State(state): State<AppState>,
    AuthUser { id: user, .. }: AuthUser,
    Form(form): Form<ConsentForm>,
) -> Response {
    let Some(Ceremony::Consent { pending, .. }) =
        state.take_ceremony(&hash_token(&form.token), now_ms())
    else {
        return (
            StatusCode::BAD_REQUEST,
            "consent expired; retry from the app",
        )
            .into_response();
    };
    // The consent must be completed by the same signed-in user who saw it.
    if pending.user != user {
        return (StatusCode::FORBIDDEN, "session changed; retry").into_response();
    }
    let sep = if pending.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    if form.decision != "approve" {
        let mut to = format!("{}{sep}error=access_denied", pending.redirect_uri);
        if let Some(s) = &pending.state {
            to.push_str(&format!("&state={}", crate::auth::urlencode(s)));
        }
        return Redirect::to(&to).into_response();
    }

    let code = new_token();
    let code_hash = hash_token(&code);
    let now = now_ms();
    let services = state.services.clone();
    let p = pending.clone();
    let result = tokio::task::spawn_blocking(move || {
        services.store().create_oauth_code(
            &code_hash,
            &p.client_id,
            p.user,
            &p.redirect_uri,
            &p.code_challenge,
            &p.scope,
            p.resource.as_deref(),
            now + CODE_TTL_MS,
        )
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        tracing::error!("create code failed: {result:?}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    let mut to = format!(
        "{}{sep}code={}",
        pending.redirect_uri,
        crate::auth::urlencode(&code)
    );
    if let Some(s) = &pending.state {
        to.push_str(&format!("&state={}", crate::auth::urlencode(s)));
    }
    Redirect::to(&to).into_response()
}

// ---- token ----

#[derive(Deserialize)]
struct TokenForm {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    grant_type: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    code: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_redirect_uri")]
    redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    client_id: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_code_verifier")]
    code_verifier: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    refresh_token: Option<String>,
}

fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({"error": code, "error_description": description})),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn token_response(access: &str, refresh: &str, scope: &str) -> Response {
    let mut response = Json(serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_MS / 1000,
        "refresh_token": refresh,
        "scope": scope,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(client_id), Some(verifier)) =
                (form.code, form.client_id, form.code_verifier)
            else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "missing fields");
            };
            let services = state.services.clone();
            let now = now_ms();
            let code_hash = hash_token(&code);
            let access = new_token();
            let refresh = new_token();
            let access_hash = hash_token(&access);
            let refresh_hash = hash_token(&refresh);
            let redirect_uri = form.redirect_uri;
            // On success: the scope, plus who authorised which client so
            // the connection can be counted per account.
            type Issued = (String, UserId, String);
            let result = tokio::task::spawn_blocking(
                move || -> Result<Result<Issued, &'static str>, flash_store::StoreError> {
                    match services.store().consume_oauth_code(&code_hash, now)? {
                        CodeConsume::Invalid => Ok(Err("invalid_grant")),
                        CodeConsume::Replayed { client_id, user_id } => {
                            // OAuth 2.1: code replay revokes the whole grant.
                            services.store().revoke_grant(&client_id, user_id, now)?;
                            tracing::warn!("authorization code replay detected; grant revoked");
                            Ok(Err("invalid_grant"))
                        }
                        CodeConsume::Fresh(grant) => {
                            if grant.client_id != client_id {
                                return Ok(Err("invalid_grant"));
                            }
                            if redirect_uri.as_deref() != Some(grant.redirect_uri.as_str()) {
                                return Ok(Err("invalid_grant"));
                            }
                            if pkce_s256(&verifier) != grant.code_challenge {
                                return Ok(Err("invalid_grant"));
                            }
                            services.store().insert_oauth_token(
                                &access_hash,
                                &refresh_hash,
                                &grant.client_id,
                                grant.user_id,
                                &grant.scope,
                                "",
                                now + ACCESS_TTL_MS,
                                now + REFRESH_TTL_MS,
                                now,
                            )?;
                            let client_name = services
                                .store()
                                .get_oauth_client(&grant.client_id)?
                                .map(|c| c.client_name)
                                .unwrap_or_default();
                            Ok(Ok((grant.scope, grant.user_id, client_name)))
                        }
                    }
                },
            )
            .await;
            match result {
                Ok(Ok(Ok((scope, user, client_name)))) => {
                    state.ext.account_event(
                        &state.services,
                        user,
                        "connector_authorized",
                        Surface::Mcp,
                        Some(&client_name),
                        None,
                    );
                    token_response(&access, &refresh, &scope)
                }
                Ok(Ok(Err(code))) => oauth_error(StatusCode::BAD_REQUEST, code, "grant rejected"),
                other => {
                    tracing::error!("token exchange: {other:?}");
                    oauth_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "internal",
                    )
                }
            }
        }
        Some("refresh_token") => {
            let Some(refresh_token) = form.refresh_token else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "missing refresh_token",
                );
            };
            let services = state.services.clone();
            let now = now_ms();
            let old_hash = hash_token(&refresh_token);
            let access = new_token();
            let refresh = new_token();
            let access_hash = hash_token(&access);
            let refresh_hash = hash_token(&refresh);
            let result = tokio::task::spawn_blocking(move || {
                services.store().rotate_refresh_token(
                    &old_hash,
                    &access_hash,
                    &refresh_hash,
                    now + ACCESS_TTL_MS,
                    now + REFRESH_TTL_MS,
                    now,
                )
            })
            .await;
            match result {
                Ok(Ok(Some(grant))) => token_response(&access, &refresh, &grant.scope),
                Ok(Ok(None)) => oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "refresh token invalid",
                ),
                other => {
                    tracing::error!("refresh: {other:?}");
                    oauth_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "server_error",
                        "internal",
                    )
                }
            }
        }
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}

// ---- resource-server middleware for /mcp ----

/// The registered name of the client behind a bearer request ("Claude",
/// "ChatGPT", …), alongside the `UserId` in request extensions.
#[derive(Debug, Clone)]
pub struct ConnectorClient(pub String);

/// Validates `Authorization: Bearer` and injects UserId (and the client's
/// name) into request extensions (rmcp forwards them to tool handlers).
/// 401s carry the resource-metadata pointer that MCP clients use to
/// discover OAuth.
pub async fn require_bearer(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let info = match token {
        Some(token) => {
            let hash = hash_token(&token);
            let services = state.services.clone();
            tokio::task::spawn_blocking(move || services.store().lookup_api_token(&hash, now_ms()))
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten()
                // A token minted for the first-party app drives the API,
                // not MCP; the two grants are different consents.
                .filter(|info| info.client_id != crate::api::MOBILE_CLIENT_ID)
        }
        None => None,
    };
    match info {
        Some(info) => {
            request.extensions_mut().insert(info.user);
            request
                .extensions_mut()
                .insert(ConnectorClient(info.client_name));
            next.run(request).await
        }
        None => {
            let metadata_url = format!(
                "{}/.well-known/oauth-protected-resource",
                state.config.base_url
            );
            let mut response =
                (StatusCode::UNAUTHORIZED, "authentication required").into_response();
            if let Ok(value) =
                format!("Bearer resource_metadata=\"{metadata_url}\"").parse::<HeaderValue>()
            {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
            response
        }
    }
}
