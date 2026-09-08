//! Email+password auth against the real router: enroll chooser, login,
//! lockout, reset flow, and settings management. A self-serve signup
//! loop and its captcha belong to a downstream extension and are
//! tested there.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::auth::{hash_token, new_token, urlencode};
use flash_server::email::{CaptureMailer, OutgoingEmail};
use flash_server::service::now_ms;
use flash_store::Store;

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    mailer: Arc<CaptureMailer>,
}

fn harness() -> Harness {
    let mailer = Arc::new(CaptureMailer::default());
    let t = AppBuilder::new("password").mailer(mailer.clone()).build();
    Harness {
        app: t.app,
        store: t.store,
        mailer,
    }
}

async fn send(
    app: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, String) {
    let r = common::send(app, request).await;
    let text = r.text();
    (r.status, r.headers, text)
}

/// Anonymous same-origin form post.
fn form_post(path: &str, body: String) -> Request<Body> {
    common::form_post(path, None, &body)
}

fn session_cookie_of(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| v.clone())
        .find(|v| v.starts_with("__Host-flash="))
        .map(|v| v.split(';').next().unwrap().to_string())
}

/// Creates a password account from an admin invite through the enroll
/// chooser and returns its session cookie.
async fn enroll_with_password(h: &Harness, email: &str, password: &str) -> String {
    let token = new_token();
    h.store
        .create_invite(
            &hash_token(&token),
            "Tester",
            "member",
            None,
            now_ms() + 86_400_000,
            now_ms(),
        )
        .unwrap();
    let (status, headers, body) = send(
        &h.app,
        form_post(
            &format!("/auth/enroll/{token}/password"),
            format!(
                "email={}&password={}&confirm={}",
                urlencode(email),
                urlencode(password),
                urlencode(password)
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    session_cookie_of(&headers).expect("session cookie after password enroll")
}

async fn login(h: &Harness, email: &str, password: &str) -> (StatusCode, Option<String>, String) {
    let (status, headers, body) = send(
        &h.app,
        form_post(
            "/auth/login/password",
            format!(
                "email={}&password={}",
                urlencode(email),
                urlencode(password)
            ),
        ),
    )
    .await;
    (status, session_cookie_of(&headers), body)
}

async fn page_with(h: &Harness, path: &str, cookie: &str) -> StatusCode {
    let (status, _, _) = send(
        &h.app,
        Request::get(path)
            .header(header::COOKIE, cookie.to_string())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    status
}

#[tokio::test]
async fn password_enroll_creates_the_account_and_burns_the_invite() {
    let h = harness();
    let cookie = enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;
    assert_eq!(page_with(&h, "/settings", &cookie).await, StatusCode::OK);
    assert!(h.store.user_by_email("pw@example.com").unwrap().is_some());
    // The invite burned: a second submit of any password enroll 403s.
    let (status, _, _) = send(
        &h.app,
        form_post(
            "/auth/enroll/nonexistent-token/password",
            "email=x@example.com&password=hunter42hunter42&confirm=hunter42hunter42".to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_success_failure_and_uniformity() {
    let h = harness();
    enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;

    let (status, cookie, _) = login(&h, "pw@example.com", "hunter42hunter42").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = cookie.expect("session cookie");
    assert_eq!(page_with(&h, "/", &cookie).await, StatusCode::OK);

    // Wrong password vs unknown email: byte-identical bodies except the
    // echoed email value.
    let (s1, c1, b1) = login(&h, "pw@example.com", "wrong-password!").await;
    let (s2, c2, b2) = login(&h, "ghost@example.com", "wrong-password!").await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert!(c1.is_none() && c2.is_none());
    assert!(b1.contains("Incorrect email or password."));
    assert_eq!(
        b1.replace("pw@example.com", "X"),
        b2.replace("ghost@example.com", "X"),
        "anti-enumeration: bodies must match"
    );
}

#[tokio::test]
async fn lockout_blocks_even_correct_password_then_expires() {
    let h = harness();
    enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;
    for _ in 0..5 {
        let (_, cookie, _) = login(&h, "pw@example.com", "wrong-password!").await;
        assert!(cookie.is_none());
    }
    let (_, cookie, body) = login(&h, "pw@example.com", "hunter42hunter42").await;
    assert!(cookie.is_none(), "locked out");
    assert!(
        body.contains("Incorrect email or password."),
        "uniform message"
    );

    // Age the lockout window out; login succeeds again.
    h.store
        .raw_execute_for_tests(&format!(
            "UPDATE users SET pw_failed_at = {} WHERE email = 'pw@example.com'",
            now_ms() - flash_store::PW_LOCKOUT_WINDOW_MS - 1
        ))
        .unwrap();
    let (_, cookie, _) = login(&h, "pw@example.com", "hunter42hunter42").await;
    assert!(cookie.is_some());
}

#[tokio::test]
async fn reset_flow_end_to_end() {
    let h = harness();
    let old_cookie = enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;

    // Request: known email -> mail; unknown -> same page, no mail.
    let before = h.mailer.0.lock().len();
    let (status, _, body) = send(
        &h.app,
        form_post("/reset", "email=pw%40example.com".to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Check your email"));
    let (_, _, body2) = send(
        &h.app,
        form_post("/reset", "email=ghost%40example.com".to_string()),
    )
    .await;
    assert!(body2.contains("Check your email"));
    let mails: Vec<OutgoingEmail> = h.mailer.0.lock().clone();
    assert_eq!(mails.len(), before + 1, "exactly one reset mail");

    // Cooldown: immediate second request sends nothing.
    send(
        &h.app,
        form_post("/reset", "email=pw%40example.com".to_string()),
    )
    .await;
    assert_eq!(h.mailer.0.lock().len(), before + 1);

    let mail = mails.last().unwrap();
    let marker = format!("{BASE}/reset/");
    let start = mail.text.find(&marker).unwrap() + marker.len();
    let token: String = mail.text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();

    // Confirm page renders; mismatched passwords bounce.
    let (status, _, _) = send(
        &h.app,
        Request::get(format!("/reset/{token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = send(
        &h.app,
        form_post(
            &format!("/auth/reset/{token}"),
            "password=newpassword42&confirm=different42".to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("match"), "{body}");

    // Real confirm: new session issued, old session dead, token burned.
    let (status, headers, _) = send(
        &h.app,
        form_post(
            &format!("/auth/reset/{token}"),
            "password=newpassword42&confirm=newpassword42".to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let new_cookie = session_cookie_of(&headers).unwrap();
    assert_eq!(
        page_with(&h, "/settings", &new_cookie).await,
        StatusCode::OK
    );
    assert_eq!(
        page_with(&h, "/settings", &old_cookie).await,
        StatusCode::SEE_OTHER,
        "pre-reset session must be dead"
    );
    let (status, _, _) = send(
        &h.app,
        Request::get(format!("/reset/{token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "token single-use");

    // Old password no longer works; new one does.
    let (_, cookie, _) = login(&h, "pw@example.com", "hunter42hunter42").await;
    assert!(cookie.is_none());
    let (_, cookie, _) = login(&h, "pw@example.com", "newpassword42").await;
    assert!(cookie.is_some());
}

#[tokio::test]
async fn reset_404s_without_mailer() {
    let app = AppBuilder::new("password-nomailer").build().app;
    let (status, _, _) = send(&app, Request::get("/reset").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn settings_change_keeps_current_session_kills_others() {
    let h = harness();
    enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;
    let (_, c1, _) = login(&h, "pw@example.com", "hunter42hunter42").await;
    let (_, c2, _) = login(&h, "pw@example.com", "hunter42hunter42").await;
    let (c1, c2) = (c1.unwrap(), c2.unwrap());

    // Change from session c1 with wrong current password -> pwerr.
    let (status, headers, _) = send(
        &h.app,
        Request::post("/settings/password/change")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, c1.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "current=wrong!&password=newpassword42&confirm=newpassword42",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(header_of(&headers, "location").unwrap().contains("pwerr=1"));

    // Correct change: c1 survives, c2 dies.
    let (status, headers, _) = send(
        &h.app,
        Request::post("/settings/password/change")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, c1.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "current=hunter42hunter42&password=newpassword42&confirm=newpassword42",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(header_of(&headers, "location").unwrap().contains("pw=1"));
    assert_eq!(page_with(&h, "/settings", &c1).await, StatusCode::OK);
    assert_eq!(page_with(&h, "/settings", &c2).await, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn remove_password_requires_a_passkey() {
    let h = harness();
    let cookie = enroll_with_password(&h, "pw@example.com", "hunter42hunter42").await;
    // Password-only account: removal refused.
    let (status, headers, _) = send(
        &h.app,
        Request::post("/settings/password/remove")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("current=hunter42hunter42"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(header_of(&headers, "location").unwrap().contains("pwerr=1"));

    // With a passkey on file, removal works.
    let user = h
        .store
        .user_by_email("pw@example.com")
        .unwrap()
        .unwrap()
        .user;
    h.store
        .add_passkey(user, b"cred", "{}", "test", now_ms())
        .unwrap();
    let (status, headers, _) = send(
        &h.app,
        Request::post("/settings/password/remove")
            .header(header::ORIGIN, BASE)
            .header(header::COOKIE, cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("current=hunter42hunter42"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(header_of(&headers, "location").unwrap().contains("pw=1"));
    assert!(h.store.get_password_hash(user).unwrap().is_none());
}
