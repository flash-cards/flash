//! Two guards on the MCP surface that hold by construction:
//!
//! - every tool annotated destructive takes a `confirm_token` and checks
//!   it, so no single call, however it was prompted (a card's text is
//!   handed to the model verbatim), can delete anything: the first call
//!   records the request and the second must carry the token the
//!   person's yes earned. The scan reads the tool declarations, so a new
//!   destructive tool without the two-step shape fails the build, and so
//!   does a tool that deletes without declaring itself destructive;
//! - the body limit the code names is the one the transport enforces.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::*;
use flash_scan::crate_at;
use flash_server::mcp::MCP_BODY_LIMIT;

#[test]
fn every_destructive_tool_takes_and_checks_a_confirm_token() {
    let file = crate_at(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).file("src/mcp.rs");
    let mut destructive = Vec::new();
    let mut offenders = Vec::new();
    for tool in file.functions.iter().filter(|f| f.attr("tool").is_some()) {
        let name = &tool.name;
        let in_body = |f: &dyn Fn(&str) -> bool| {
            file.calls
                .iter()
                .filter(|c| c.function.as_deref() == Some(name))
                .any(|c| c.path.last().is_some_and(|l| f(l)))
                || file
                    .method_calls
                    .iter()
                    .filter(|m| m.function.as_deref() == Some(name))
                    .any(|m| f(&m.method))
        };
        let declared_destructive = tool
            .attr("tool")
            .is_some_and(|a| a.args.contains("destructive_hint=true"));
        // A tool that deletes without saying so is a destructive tool
        // the scan would otherwise never look at.
        if in_body(&|callee| callee.starts_with("delete_")) && !declared_destructive {
            offenders.push(format!(
                "{name} calls a delete without `destructive_hint = true`"
            ));
        }
        if !declared_destructive {
            continue;
        }
        destructive.push(name.clone());
        // The token in the arguments is worth nothing unless the body
        // checks it before acting.
        let confirms = file.method_calls.iter().any(|m| {
            m.function.as_deref() == Some(name) && m.method == "confirmed" && m.receiver == "self"
        });
        if !confirms {
            offenders.push(format!("{name} never calls self.confirmed(…)"));
        }
        // The arguments type, `Parameters<X>`, must carry the token.
        let args_type = tool
            .params
            .iter()
            .flat_map(|p| p.ty.idents.iter())
            .skip_while(|i| *i != "Parameters")
            .nth(1)
            .cloned()
            .unwrap_or_default();
        let has_token = file.type_named(&args_type).is_some_and(|t| {
            t.fields
                .iter()
                .any(|f| f.name.as_deref() == Some("confirm_token"))
        });
        if !has_token {
            offenders.push(format!(
                "{name} takes {args_type}, which has no confirm_token"
            ));
        }
    }
    assert!(
        destructive.len() >= 2,
        "the scan found too few destructive tools to be trusted: {destructive:?}"
    );
    assert!(
        offenders.is_empty(),
        "destructive tools without the two-step shape:\n{}",
        offenders.join("\n")
    );
}

#[tokio::test]
async fn the_transport_enforces_the_named_body_limit() {
    let t = AppBuilder::new("mcp-body-limit").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    let bearer = oauth_bearer(&t.store, user);
    let padding = "x".repeat(MCP_BODY_LIMIT + 1024);
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"{padding}","version":"0"}}}}}}"#
    );
    let reply = send(
        &t.app,
        Request::post("/mcp")
            .header(header::HOST, "localhost:8437")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{} {}",
        reply.status,
        reply.text()
    );
}
