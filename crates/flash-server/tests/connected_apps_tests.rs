//! The Connected apps card on the settings page: every client holding a
//! live grant is listed (connectors by their registered name, the
//! first-party app by device count), and disconnecting one revokes all
//! of that client's tokens for this user and nobody else's.

use axum::http::StatusCode;
use flash_server::api::MOBILE_CLIENT_ID;
use flash_server::auth::hash_token;
use flash_server::service::now_ms;
use flash_server::testing::{
    api_bearer, form_post, get, oauth_bearer, send, signed_in, AppBuilder,
};

#[tokio::test]
async fn the_card_lists_every_client_and_disconnecting_revokes_its_tokens() {
    let t = AppBuilder::new("connected-apps").build();
    let (user, cookie) = signed_in(&t.store, "FlashTester", "flashtester@example.com");
    let (_other, other_cookie) = signed_in(&t.store, "Other", "other@example.com");
    let connector = oauth_bearer(&t.store, user);
    let phone = api_bearer(&t.store, user);
    let tablet = api_bearer(&t.store, user);

    let page = send(&t.app, get("/settings", Some(&cookie))).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.text();
    assert!(html.contains("Connected apps"));
    assert!(html.contains("Flash app"), "the first-party app is listed");
    assert!(
        html.contains("2 devices"),
        "one row per client, devices counted"
    );
    assert!(
        html.contains("Connector"),
        "a client without a registration row still shows"
    );

    // Someone else cannot disconnect this user's app.
    let foreign = send(
        &t.app,
        form_post(
            "/settings/apps/disconnect",
            Some(&other_cookie),
            &format!("client_id={MOBILE_CLIENT_ID}"),
        ),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::SEE_OTHER);
    assert!(t
        .store
        .lookup_access_token(&hash_token(&phone), now_ms())
        .unwrap()
        .is_some());

    // The owner can, and it takes every device with it.
    let reply = send(
        &t.app,
        form_post(
            "/settings/apps/disconnect",
            Some(&cookie),
            &format!("client_id={MOBILE_CLIENT_ID}"),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/settings?saved=apps");
    for token in [&phone, &tablet] {
        assert!(t
            .store
            .lookup_access_token(&hash_token(token), now_ms())
            .unwrap()
            .is_none());
    }
    // The connector's grant is untouched.
    assert!(t
        .store
        .lookup_access_token(&hash_token(&connector), now_ms())
        .unwrap()
        .is_some());

    let html = send(&t.app, get("/settings?saved=apps", Some(&cookie)))
        .await
        .text();
    assert!(!html.contains("Flash app"));
    assert!(html.contains("Connector"));
    assert!(html.contains("Disconnected"));
}

#[tokio::test]
async fn an_account_with_nothing_connected_says_so() {
    let t = AppBuilder::new("connected-apps-empty").build();
    let (_user, cookie) = signed_in(&t.store, "FlashTester", "flashtester@example.com");
    let html = send(&t.app, get("/settings", Some(&cookie))).await.text();
    assert!(html.contains("Nothing connected."));
}
