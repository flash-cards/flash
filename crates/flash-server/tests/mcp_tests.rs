//! End-to-end MCP tests over the real Streamable HTTP endpoint: initialize,
//! tool discovery, and a complete voice-style study loop, authenticated
//! with a real bearer token.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_server::service::now_ms;
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    bearer: String,
    store: std::sync::Arc<flash_store::Store>,
}

fn harness() -> Harness {
    let t = AppBuilder::new("mcp").build();
    let user = t
        .store
        .create_user("Tester", None, "member", now_ms())
        .unwrap();
    // The bearer helper mints tokens for "test-client"; register it under
    // the name a real MCP client would carry.
    t.store
        .create_oauth_client("test-client", "Claude", &[], "{}", now_ms())
        .unwrap();
    Harness {
        bearer: oauth_bearer(&t.store, user),
        store: t.store.clone(),
        app: t.app,
    }
}

/// The `client` column of every review, oldest first.
fn review_clients(store: &flash_store::Store) -> Vec<Option<String>> {
    store
        .with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT client FROM review_log ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

/// Sends one JSON-RPC message; returns (status, session_id_header, body_text).
async fn rpc(
    h: &Harness,
    session: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>, String) {
    let mut request = Request::post("/mcp")
        .header(header::HOST, "localhost:8437")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.bearer))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = h
        .app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, session, String::from_utf8_lossy(&bytes).to_string())
}

/// Extracts the JSON-RPC result from a plain-JSON or SSE-framed body.
fn result_of(body: &str) -> serde_json::Value {
    // SSE streams open with a priming frame (`data:` + retry); take the
    // first data line that actually carries JSON.
    let json_part = if body.contains("data:") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|payload| payload.starts_with('{'))
            .unwrap_or("{}")
            .to_string()
    } else {
        body.to_string()
    };
    let value: serde_json::Value = serde_json::from_str(&json_part).unwrap_or_default();
    value["result"].clone()
}

/// The tool result's text content, parsed as JSON.
fn tool_json(result: &serde_json::Value) -> serde_json::Value {
    let text = result["content"][0]["text"].as_str().unwrap_or("{}");
    serde_json::from_str(text).unwrap_or_default()
}

async fn initialize(h: &Harness) -> String {
    let (status, session, body) = rpc(
        h,
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result = result_of(&body);
    assert!(
        result["instructions"]
            .as_str()
            .unwrap_or("")
            .contains("study"),
        "server instructions present; raw body: {body}"
    );
    let session = session.expect("session id assigned");
    // Complete the handshake.
    let (status, _, _) = rpc(
        h,
        Some(&session),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(status.is_success() || status == StatusCode::ACCEPTED);
    session
}

async fn call_tool(
    h: &Harness,
    session: &str,
    id: i64,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let (status, _, body) = rpc(
        h,
        Some(session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result = result_of(&body);
    assert_ne!(result, serde_json::Value::Null, "rpc error: {body}");
    tool_json(&result)
}

#[tokio::test]
async fn tools_are_discoverable() {
    let h = harness();
    let session = initialize(&h).await;
    let (status, _, body) = rpc(
        &h,
        Some(&session),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tools = result_of(&body)["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "start_study_session",
        "submit_review",
        "show_answer",
        "end_session",
        "list_decks",
        "create_deck",
        "create_cards",
        "update_card",
        "delete_card",
        "delete_deck",
        "list_cards",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}: {names:?}"
        );
    }

    // Every tool advertises an outputSchema (derived from its Json<T> return).
    for t in tools.as_array().unwrap() {
        let name = t["name"].as_str().unwrap_or("?");
        assert!(t["outputSchema"].is_object(), "{name} has no outputSchema");
    }
    let start = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "start_study_session")
        .unwrap();
    let schema = start["outputSchema"].to_string();
    assert!(schema.contains("cards_due"), "{schema}");
    assert!(schema.contains("first_card"), "{schema}");
}

#[tokio::test]
async fn full_voice_study_loop() {
    let h = harness();
    let session = initialize(&h).await;

    // Claude turns pasted notes into cards.
    let created = call_tool(
        &h,
        &session,
        10,
        "create_cards",
        serde_json::json!({
            "deck": "Pharm",
            "cards": [
                {"front": "Antidote for warfarin?", "back": "Vitamin K", "tags": ["exam-2"]},
                {"front": "Antidote for heparin?", "back": "Protamine sulfate"}
            ]
        }),
    )
    .await;
    assert_eq!(created["created"], 2);

    // Start a session; grading mode defaults to silent.
    let started = call_tool(
        &h,
        &session,
        11,
        "start_study_session",
        serde_json::json!({"deck": "Pharm"}),
    )
    .await;
    assert_eq!(started["cards_due"], 2);
    assert!(started["grading_mode"]
        .as_str()
        .unwrap()
        .starts_with("silent"));
    let sid = started["session_id"].as_i64().unwrap();
    let card1 = started["first_card"]["card_id"].as_i64().unwrap();

    // She answers correctly -> silent Good; next card arrives in-call.
    let r1 = call_tool(
        &h,
        &session,
        12,
        "submit_review",
        serde_json::json!({"session_id": sid, "card_id": card1, "rating": 3}),
    )
    .await;
    let card2 = r1["next_card"]["card_id"].as_i64().unwrap();
    assert_ne!(card2, card1);

    // She gives up on card 2: show_answer, then Again.
    let answer = call_tool(
        &h,
        &session,
        13,
        "show_answer",
        serde_json::json!({"card_id": card2}),
    )
    .await;
    assert_eq!(answer["back"], "Protamine sulfate");
    let r2 = call_tool(
        &h,
        &session,
        14,
        "submit_review",
        serde_json::json!({"session_id": sid, "card_id": card2, "rating": 1}),
    )
    .await;
    // The failed card is in a learning step; it's the only thing left.
    assert_eq!(r2["next_card"]["card_id"].as_i64().unwrap(), card2);

    let summary = call_tool(
        &h,
        &session,
        15,
        "end_session",
        serde_json::json!({"session_id": sid}),
    )
    .await;
    assert_eq!(summary["reviewed"], 2);
    assert_eq!(summary["good"], 1);
    assert_eq!(summary["again"], 1);

    // Each review remembers which client it came through.
    assert_eq!(
        review_clients(&h.store),
        [Some("Claude".to_string()), Some("Claude".to_string())]
    );

    // Response hygiene: nothing scheduling-ish in the submit payloads.
    for payload in [&r1, &r2] {
        let text = payload.to_string();
        assert!(!text.contains("stability"));
        assert!(!text.contains("interval"));
        assert!(!text.contains("due"));
    }
}

/// The "I just made 44 cards, quiz me" case: the daily new-card limit
/// caps the session, and the model must be told the rest are held back
/// (and how to release them) rather than left to declare the user done
/// or improvise a quiz from chat.
#[tokio::test]
async fn empty_queue_explains_daily_limit() {
    let h = harness();
    let session = initialize(&h).await;

    call_tool(
        &h,
        &session,
        20,
        "set_limits",
        serde_json::json!({"new_per_day": 1}),
    )
    .await;
    let created = call_tool(
        &h,
        &session,
        21,
        "create_cards",
        serde_json::json!({
            "deck": "Weak Spots",
            "cards": [
                {"front": "Q1", "back": "A1"},
                {"front": "Q2", "back": "A2"},
                {"front": "Q3", "back": "A3"}
            ]
        }),
    )
    .await;
    assert_eq!(created["created"], 3);

    let started = call_tool(
        &h,
        &session,
        22,
        "start_study_session",
        serde_json::json!({"deck": "Weak Spots"}),
    )
    .await;
    assert_eq!(started["cards_due"], 1);
    assert!(started["note"].is_null(), "no note while cards remain");
    let sid = started["session_id"].as_i64().unwrap();
    let card1 = started["first_card"]["card_id"].as_i64().unwrap();

    let r = call_tool(
        &h,
        &session,
        23,
        "submit_review",
        serde_json::json!({"session_id": sid, "card_id": card1, "rating": 3}),
    )
    .await;
    assert!(r["next_card"].is_null());
    assert_eq!(r["remaining"], 0);
    let note = r["note"].as_str().expect("note explains the empty queue");
    assert!(note.contains("2 more card"), "{note}");
    assert!(note.contains("boost_new_today"), "{note}");

    // Following the note releases the rest.
    call_tool(
        &h,
        &session,
        24,
        "boost_new_today",
        serde_json::json!({"deck": "Weak Spots", "extra": 5}),
    )
    .await;
    let again = call_tool(
        &h,
        &session,
        25,
        "start_study_session",
        serde_json::json!({"deck": "Weak Spots"}),
    )
    .await;
    assert_eq!(again["cards_due"], 2);
    assert!(again["note"].is_null());
}

#[tokio::test]
async fn delete_deck_removes_the_deck_and_reports_card_count() {
    let h = harness();
    let session = initialize(&h).await;
    let created = call_tool(
        &h,
        &session,
        40,
        "create_cards",
        serde_json::json!({
            "deck": "Ephemeral",
            "cards": [
                {"front": "q1", "back": "a1"},
                {"front": "q2", "back": "a2"}
            ]
        }),
    )
    .await;
    assert_eq!(created["created"], 2);

    // Step one deletes nothing: it names what would go and hands back a
    // token the model can only spend after the person said yes.
    let asked = call_tool(
        &h,
        &session,
        41,
        "delete_deck",
        serde_json::json!({"deck": "Ephemeral"}),
    )
    .await;
    assert_eq!(asked["deleted"], false);
    assert_eq!(asked["pending"], true);
    assert_eq!(asked["cards_affected"], 2);
    let token = asked["confirm_token"]
        .as_str()
        .expect("confirm token")
        .to_string();
    let decks = call_tool(&h, &session, 42, "list_decks", serde_json::json!({})).await;
    assert!(decks["decks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["name"] == "Ephemeral"));

    // A token for one target does not spend on another.
    let (status, _, body) = rpc(
        &h,
        Some(&session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 43, "method": "tools/call",
            "params": {"name": "delete_card", "arguments": {"card_id": 1, "confirm_token": token}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        result_of(&body) == serde_json::Value::Null || result_of(&body)["isError"] == true,
        "a deck token spent on a card: {body}"
    );

    // Step two, with the token, deletes; the token is then spent.
    let deleted = call_tool(
        &h,
        &session,
        44,
        "delete_deck",
        serde_json::json!({"deck": "Ephemeral", "confirm_token": token}),
    )
    .await;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["cards_affected"], 2);

    let decks = call_tool(&h, &session, 45, "list_decks", serde_json::json!({})).await;
    let names: Vec<&str> = decks["decks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(!names.contains(&"Ephemeral"), "{names:?}");
}

/// A token minted for the first-party app is not an MCP grant: the two
/// are different consents, and the app's token must not drive tools.
#[tokio::test]
async fn a_mobile_app_token_does_not_authenticate_mcp() {
    let t = AppBuilder::new("mcp-token-class").build();
    let user = member(&t.store, "FlashTester", "flashtester@example.com");
    let h = Harness {
        bearer: api_bearer(&t.store, user),
        store: t.store.clone(),
        app: t.app,
    };
    let (status, _, _) = rpc(
        &h,
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Ids in tool arguments are scoped by the token's user: another user's
/// session and card ids are unknown, and nothing of theirs moves.
#[tokio::test]
async fn tool_arguments_cannot_reach_another_users_cards() {
    let t = AppBuilder::new("mcp-isolation").build();
    let owner = member(&t.store, "FlashTester", "flashtester@example.com");
    let other = member(&t.store, "Other", "other@example.com");
    t.store
        .create_oauth_client("test-client", "Claude", &[], "{}", now_ms())
        .unwrap();
    let now = now_ms();
    let deck = t.services.create_deck(owner, "Owned", "", now).unwrap();
    let cards = t
        .services
        .create_cards_in_deck(owner, deck, &[("f".into(), "b".into(), vec![])], now)
        .unwrap();
    let session = t
        .services
        .start_session(owner, flash_core::queue::StudyScope::All, now)
        .unwrap()
        .session_id;
    let h = Harness {
        bearer: oauth_bearer(&t.store, other),
        store: t.store.clone(),
        app: t.app,
    };
    let mcp = initialize(&h).await;

    for (id, name, args) in [
        (
            80,
            "submit_review",
            serde_json::json!({"session_id": session.0, "card_id": cards[0].0, "rating": 3}),
        ),
        (
            81,
            "show_answer",
            serde_json::json!({"card_id": cards[0].0}),
        ),
        (
            82,
            "update_card",
            serde_json::json!({"card_id": cards[0].0, "front": "x", "back": "y"}),
        ),
        (
            83,
            "delete_card",
            serde_json::json!({"card_id": cards[0].0}),
        ),
        (
            84,
            "end_session",
            serde_json::json!({"session_id": session.0}),
        ),
    ] {
        let (status, _, body) = rpc(
            &h,
            Some(&mcp),
            serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": args}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        let result = result_of(&body);
        let refused = result == serde_json::Value::Null || result["isError"] == true;
        assert!(refused, "{name} answered another user's ids: {body}");
    }
    let card = t.services.card(owner, cards[0]).unwrap();
    assert_eq!((card.front.as_str(), card.back.as_str()), ("f", "b"));
    assert!(review_clients(&t.store).is_empty());
}
