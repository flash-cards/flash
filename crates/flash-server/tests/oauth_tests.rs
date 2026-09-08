//! OAuth 2.1 flow tests against the real router via tower::oneshot:
//! DCR, PKCE happy/sad paths, single-use codes with replay revocation,
//! refresh rotation, and MCP bearer discovery.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine;
use common::*;
use flash_server::service::now_ms;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    session_cookie: String,
}

fn harness() -> Harness {
    let t = AppBuilder::new("oauth").build();
    let user = t
        .store
        .create_user("Tester", None, "member", now_ms())
        .unwrap();
    Harness {
        session_cookie: web_session(&t.store, user),
        app: t.app,
    }
}

/// (status, headers, JSON body — Null when the body isn't JSON).
async fn send(
    app: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, serde_json::Value) {
    let r = common::send(app, request).await;
    let json = r.json();
    (r.status, r.headers, json)
}

async fn register_client(h: &Harness) -> String {
    let (status, _, body) = send(
        &h.app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "client_name": "Test Client",
                    "redirect_uris": ["https://client.example.com/callback"],
                    "token_endpoint_auth_method": "none"
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["client_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn registration_is_small_and_keeps_only_what_it_uses() {
    let h = harness();
    // Anything a client sends beyond the two fields we act on is accepted
    // and dropped; a body that could only be padding is refused outright.
    let padded = serde_json::json!({
        "client_name": "Padded",
        "redirect_uris": ["https://client.example.com/callback"],
        "software_statement": "x".repeat(20 * 1024),
    })
    .to_string();
    let (status, _, _) = send(
        &h.app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(padded))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // Control characters cannot reach the consent page through the name.
    let (status, _, body) = send(
        &h.app,
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "client_name": "Cla\u{0}ude\n\r",
                    "redirect_uris": ["https://client.example.com/callback"],
                    "logo_uri": "https://client.example.com/logo.png",
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["client_name"], "Claude");
    assert!(body.get("logo_uri").is_none());
}

/// Runs authorize + consent, returns the authorization code.
async fn get_code(h: &Harness, client_id: &str, challenge: &str) -> String {
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=https%3A%2F%2Fclient.example.com%2Fcallback\
         &state=xyz&code_challenge={challenge}&code_challenge_method=S256"
    );
    let (status, _, _) = send(
        &h.app,
        Request::get(&uri)
            .header(header::COOKIE, &h.session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "consent page should render");

    // The consent token is server-side; extract it from the rendered form.
    let response = h
        .app
        .clone()
        .oneshot(
            Request::get(&uri)
                .header(header::COOKIE, &h.session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let token = html
        .split("name=\"token\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("consent token in page")
        .to_string();

    let (status, headers, _) = send(
        &h.app,
        Request::post("/oauth/consent")
            .header(header::COOKIE, &h.session_cookie)
            .header(header::ORIGIN, BASE)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("token={token}&decision=approve")))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = header_of(&headers, "location").expect("redirect");
    assert!(location.contains("state=xyz"));
    location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

async fn exchange(
    h: &Harness,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = send(
        &h.app,
        Request::post("/oauth/token")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=authorization_code&code={code}\
                 &redirect_uri=https%3A%2F%2Fclient.example.com%2Fcallback\
                 &client_id={client_id}&code_verifier={verifier}"
            )))
            .unwrap(),
    )
    .await;
    (status, body)
}

fn s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[tokio::test]
async fn metadata_endpoints_are_served() {
    let h = harness();
    let (status, _, body) = send(
        &h.app,
        Request::get("/.well-known/oauth-authorization-server")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["issuer"], BASE);
    assert_eq!(body["code_challenge_methods_supported"][0], "S256");

    let (status, _, body) = send(
        &h.app,
        Request::get("/.well-known/oauth-protected-resource")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["resource"], format!("{BASE}/mcp"));
}

#[tokio::test]
async fn full_pkce_flow_and_bearer_access() {
    let h = harness();
    let client_id = register_client(&h).await;
    let verifier = "a".repeat(50);
    let code = get_code(&h, &client_id, &s256(&verifier)).await;

    let (status, body) = exchange(&h, &client_id, &code, &verifier).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let access = body["access_token"].as_str().unwrap();
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["refresh_token"].is_string());

    // Bearer token opens /mcp (a bad MCP body is fine — past the 401).
    let (status, _, _) = send(
        &h.app,
        Request::post("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {access}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_verifier_is_rejected() {
    let h = harness();
    let client_id = register_client(&h).await;
    let code = get_code(
        &h,
        &client_id,
        &s256("correct-verifier-value-123456789012345678"),
    )
    .await;
    let (status, body) = exchange(
        &h,
        &client_id,
        &code,
        "wrong-verifier-value-1234567890123456789",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn code_replay_revokes_the_grant() {
    let h = harness();
    let client_id = register_client(&h).await;
    let verifier = "b".repeat(50);
    let code = get_code(&h, &client_id, &s256(&verifier)).await;

    let (status, body) = exchange(&h, &client_id, &code, &verifier).await;
    assert_eq!(status, StatusCode::OK);
    let access = body["access_token"].as_str().unwrap().to_string();

    // Replay the same code: rejected AND the earlier token is revoked.
    let (status, _) = exchange(&h, &client_id, &code, &verifier).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, _) = send(
        &h.app,
        Request::post("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {access}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "revoked token must not work"
    );
}

#[tokio::test]
async fn refresh_rotation_works_and_old_refresh_dies() {
    let h = harness();
    let client_id = register_client(&h).await;
    let verifier = "c".repeat(50);
    let code = get_code(&h, &client_id, &s256(&verifier)).await;
    let (_, body) = exchange(&h, &client_id, &code, &verifier).await;
    let refresh1 = body["refresh_token"].as_str().unwrap().to_string();

    let refresh = |token: String| {
        let app = h.app.clone();
        async move {
            send(
                &app,
                Request::post("/oauth/token")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=refresh_token&refresh_token={token}&client_id=x"
                    )))
                    .unwrap(),
            )
            .await
        }
    };
    let (status, _, body) = refresh(refresh1.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["access_token"].is_string());

    // The rotated-out refresh token no longer works.
    let (status, _, body) = refresh(refresh1).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn mcp_401_carries_discovery_pointer() {
    let h = harness();
    let (status, headers, _) = send(
        &h.app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let www = header_of(&headers, "www-authenticate").expect("WWW-Authenticate");
    assert!(www.contains("/.well-known/oauth-protected-resource"));
}

#[tokio::test]
async fn consent_page_csp_permits_the_callback_redirect() {
    // Chrome enforces form-action against the consent POST's redirect
    // target (the client's https callback), so the consent page must
    // carry a CSP with `form-action 'self' https:`.
    let h = harness();
    let client_id = register_client(&h).await;
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=https%3A%2F%2Fclient.example.com%2Fcallback\
         &code_challenge={}&code_challenge_method=S256",
        s256(&"d".repeat(50))
    );
    let (status, headers, _) = send(
        &h.app,
        Request::get(&uri)
            .header(header::COOKIE, &h.session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let csp = header_of(&headers, "content-security-policy").expect("CSP present");
    assert!(csp.contains("form-action 'self' https:"), "got: {csp}");
}

#[tokio::test]
async fn unregistered_redirect_uri_never_redirects() {
    let h = harness();
    let client_id = register_client(&h).await;
    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}\
         &redirect_uri=https%3A%2F%2Fevil.example.com%2Fsteal\
         &code_challenge=x&code_challenge_method=S256"
    );
    let (status, headers, _) = send(
        &h.app,
        Request::get(&uri)
            .header(header::COOKIE, &h.session_cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(header_of(&headers, "location").is_none());
}

/// A malformed authorize request is a page, not a bounce. Anyone can
/// register a client with any https URI, so a redirect on error would
/// make the authorize endpoint an open redirector from this origin; the
/// redirect happens only after the person presses Approve or Deny.
#[tokio::test]
async fn a_malformed_authorize_request_is_a_page_not_a_redirect() {
    let h = harness();
    let client_id = register_client(&h).await;
    let registered = "https%3A%2F%2Fclient.example.com%2Fcallback";
    for (query, code) in [
        (
            format!("response_type=token&client_id={client_id}&redirect_uri={registered}&code_challenge=x&code_challenge_method=S256"),
            "unsupported_response_type",
        ),
        (
            format!("response_type=code&client_id={client_id}&redirect_uri={registered}&code_challenge_method=S256"),
            "invalid_request",
        ),
        (
            format!("response_type=code&client_id={client_id}&redirect_uri={registered}&code_challenge=x&code_challenge_method=plain"),
            "invalid_request",
        ),
        (
            format!("response_type=code&client_id={client_id}&redirect_uri={registered}&code_challenge=x&code_challenge_method=S256&resource=https%3A%2F%2Fother.example%2Fmcp"),
            "invalid_target",
        ),
        (
            format!("response_type=code&client_id={client_id}&redirect_uri={registered}&code_challenge=x&code_challenge_method=S256&scope=admin"),
            "invalid_scope",
        ),
    ] {
        let r = common::send(
            &h.app,
            Request::get(format!("/oauth/authorize?{query}"))
                .header(header::COOKIE, &h.session_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{query}");
        assert!(r.location().is_empty(), "{query} redirected to {}", r.location());
        assert!(r.text().contains(code), "{query}: page lacks {code}");
        assert!(!r.text().contains("client.example.com"), "{query}: page names the client's host");
    }
    // The same request with our own MCP endpoint as the resource is fine.
    let ours = "http%3A%2F%2Flocalhost%3A8437%2Fmcp";
    let r = common::send(
        &h.app,
        Request::get(format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={registered}&code_challenge=x&code_challenge_method=S256&resource={ours}"
        ))
        .header(header::COOKIE, &h.session_cookie)
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
}

/// Redirect URIs are parsed as URLs: a loopback *host* over plain http,
/// or any https host; never userinfo, a fragment, or a lookalike.
#[tokio::test]
async fn redirect_uris_are_parsed_not_prefix_matched() {
    let h = harness();
    let attempt = |uri: &str| {
        Request::post("/oauth/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "client_name": "Probe", "redirect_uris": [uri] }).to_string(),
            ))
            .unwrap()
    };
    for bad in [
        "http://localhost@evil.example/cb",
        "http://localhost.evil/cb",
        "http://evil.example/cb",
        "https://client.example/cb#frag",
        "https://user:pw@client.example/cb",
        "javascript:alert(1)",
        "https://",
        "http://127.0.0.2/cb",
    ] {
        let (status, _, body) = send(&h.app, attempt(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} accepted: {body}");
    }
    for good in [
        "http://localhost:8080/cb",
        "http://127.0.0.1/cb",
        "http://[::1]:1234/cb",
        "https://client.example/cb?x=1",
    ] {
        let (status, _, body) = send(&h.app, attempt(good)).await;
        assert_eq!(status, StatusCode::CREATED, "{good} refused: {body}");
    }
}

/// Expired authorization codes are rows a signed-in member can mint at
/// the limiter's pace; housekeeping removes them.
#[test]
fn expired_authorization_codes_are_purged() {
    let t = AppBuilder::new("oauth-purge").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    let now = now_ms();
    for (i, expires) in [(1, now - 1), (2, now + 60_000)] {
        t.store
            .create_oauth_code(
                &format!("hash-{i}"),
                "client",
                user,
                "https://client.example/cb",
                "challenge",
                "flashcards",
                None,
                expires,
            )
            .unwrap();
    }
    assert_eq!(t.store.purge_expired_oauth_codes(now).unwrap(), 1);
    assert_eq!(t.store.purge_expired_oauth_codes(now).unwrap(), 0);
}
