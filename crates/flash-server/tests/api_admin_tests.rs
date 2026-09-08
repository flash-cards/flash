//! Admin over the API (users, invites) and the Connect page. The growth
//! metrics belong to a downstream extension and are tested there.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn admin_routes_are_gated_and_shaped() {
    let t = AppBuilder::new("api-admin").build();
    let admin = t
        .store
        .create_user("Root", Some("root@example.com"), "admin", 1)
        .unwrap();
    let admin_b = api_bearer(&t.store, admin);
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let flashtester_b = api_bearer(&t.store, flashtester);

    assert_eq!(
        send(
            &t.app,
            json_get("/api/v1/admin/users", Some(&flashtester_b))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    let users = send(&t.app, json_get("/api/v1/admin/users", Some(&admin_b)))
        .await
        .json();
    let names: Vec<&str> = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["display_name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"FlashTester") && names.contains(&"Root"));

    // Invites: validated, then a one-time enroll URL the API can read.
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/admin/invites",
            Some(&admin_b),
            &json!({"display_name": " "}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = send(
        &t.app,
        json_post(
            "/api/v1/admin/invites",
            Some(&admin_b),
            &json!({"display_name": "Ada"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let url = reply.json()["url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap().to_string();
    let info = send(
        &t.app,
        json_get(&format!("/api/v1/auth/enroll/{token}"), None),
    )
    .await
    .json();
    assert_eq!(info["display_name"], "Ada");
    assert_eq!(info["email"], serde_json::Value::Null);
    assert_eq!(
        send(
            &t.app,
            json_post(
                "/api/v1/admin/invites",
                Some(&flashtester_b),
                &json!({"display_name": "X"})
            )
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn connect_page_links_carry_the_mcp_url() {
    let t = AppBuilder::new("api-connect").build();
    let flashtester = member(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, flashtester);
    let c = send(&t.app, json_get("/api/v1/connect", Some(&bearer)))
        .await
        .json();
    let mcp = c["mcp_url"].as_str().unwrap();
    assert!(mcp.ends_with("/mcp"));
    assert!(c["claude_url"]
        .as_str()
        .unwrap()
        .starts_with("https://claude.ai/new?q="));
    // The prompt is percent-encoded (spaces, colons) and names the server.
    let claude = c["claude_url"].as_str().unwrap();
    assert!(claude.contains("%20") && !claude.contains(' '));
    assert!(claude.contains("/mcp"));
    assert!(c["chatgpt_url"]
        .as_str()
        .unwrap()
        .starts_with("https://chatgpt.com/?q="));
    assert!(c["claude_desktop_url"]
        .as_str()
        .unwrap()
        .starts_with("claude://"));
    assert_eq!(
        c["claude_code_cmd"],
        format!("claude mcp add --transport http flash {mcp}")
    );
}
