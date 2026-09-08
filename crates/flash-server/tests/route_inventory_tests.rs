//! Every route the open server mounts, probed as a stranger. Two
//! properties hold for the whole inventory, not a sample of it:
//!
//! - a request with no session and no bearer is turned away everywhere
//!   except on the routes listed as the front door;
//! - every browser mutation refuses a cross-origin request before it does
//!   anything, except the protocol endpoints that authenticate by token
//!   or signature and are listed as such.
//!
//! The inventory comes from the source tree (see `testing::routes`), so
//! a new route cannot dodge either check.

use std::path::Path;

use flash_server::testing::routes::{
    identifier_sites, inventory, listed, probe, probe_as, unmatched, Probe, Route, FOREIGN_ORIGIN,
    OWN_ORIGIN,
};
use flash_server::testing::{signed_in, AppBuilder};

/// Routes that exist for people who are not signed in: the sign-in and
/// enrolment surfaces, password reset, the public assets, the OAuth
/// authorization server's own endpoints, and the API's login routes.
const FRONT_DOOR: &[(&str, &str)] = &[
    ("GET", "/robots.txt"),
    ("GET", "/login"),
    ("GET", "/connect"),
    ("GET", "/enroll/{token}"),
    ("GET", "/reset"),
    ("GET", "/reset/{token}"),
    ("GET", "/favicon.ico"),
    ("GET", "/static/*"),
    ("GET", "/.well-known/*"),
    ("POST", "/reset"),
    ("POST", "/auth/login/password"),
    ("POST", "/auth/login/start"),
    ("POST", "/auth/login/finish"),
    ("POST", "/auth/reset/{token}"),
    ("POST", "/auth/enroll/{token}/password"),
    ("POST", "/auth/enroll/{token}/start"),
    ("POST", "/auth/enroll/finish"),
    ("POST", "/oauth/register"),
    ("POST", "/oauth/token"),
    ("GET", "/api/v1/meta"),
    ("POST", "/api/v1/auth/password"),
    ("POST", "/api/v1/auth/passkey/login/start"),
    ("POST", "/api/v1/auth/passkey/login/finish"),
    ("POST", "/api/v1/auth/refresh"),
    ("POST", "/api/v1/auth/reset"),
    ("POST", "/api/v1/auth/reset/{token}"),
    ("GET", "/api/v1/auth/enroll/{token}"),
    ("POST", "/api/v1/auth/enroll/{token}/password"),
    ("POST", "/api/v1/auth/enroll/{token}/passkey/start"),
    ("POST", "/api/v1/auth/enroll/{token}/passkey/finish"),
];

/// Mutations outside the same-origin guard: the OAuth token endpoints
/// (bearer or client credentials, called by non-browsers) and MCP.
const PROTOCOL: &[(&str, &str)] = &[
    ("POST", "/oauth/register"),
    ("POST", "/oauth/token"),
    ("POST", "/mcp"),
    ("DELETE", "/mcp"),
];

fn core_routes() -> Vec<Route> {
    let routes = inventory(&[Path::new(env!("CARGO_MANIFEST_DIR"))]);
    assert!(
        routes.len() > 100,
        "inventory found {} routes",
        routes.len()
    );
    routes
}

fn report(title: &str, lines: &[String]) {
    assert!(lines.is_empty(), "{title}:\n{}", lines.join("\n"));
}

#[tokio::test]
async fn strangers_are_turned_away_everywhere_but_the_front_door() {
    let t = AppBuilder::new("route-inventory").build();
    let mut leaks = Vec::new();
    let mut stale = Vec::new();
    for route in core_routes() {
        let p: Probe = probe(&t.app, &route, OWN_ORIGIN).await;
        assert!(
            !p.status.is_server_error(),
            "a stranger crashed a route: {}",
            p.line()
        );
        if listed(&route, FRONT_DOOR) {
            if p.turned_away() {
                stale.push(p.line());
            }
        } else if !p.turned_away() {
            leaks.push(p.line());
        }
    }
    report(
        "routes that answered a stranger with something other than a sign-in demand",
        &leaks,
    );
    report(
        "front-door routes that now demand a sign-in (remove them from the list)",
        &stale,
    );
}

#[tokio::test]
async fn browser_mutations_refuse_cross_origin_requests() {
    let t = AppBuilder::new("route-inventory-csrf").build();
    let mut unguarded = Vec::new();
    let mut stale = Vec::new();
    for route in core_routes()
        .into_iter()
        .filter(|r| r.is_mutation() && !r.is_api())
    {
        let p = probe(&t.app, &route, FOREIGN_ORIGIN).await;
        if listed(&route, PROTOCOL) {
            if p.refused_cross_origin() {
                stale.push(p.line());
            }
        } else if !p.refused_cross_origin() {
            unguarded.push(p.line());
        }
    }
    report(
        "browser mutations that accepted a cross-origin request",
        &unguarded,
    );
    report(
        "protocol routes that are behind the same-origin guard after all",
        &stale,
    );
}

/// The API authenticates bearers only. A browser's session cookie must
/// buy nothing there, or a page on another site could drive the API
/// through the visitor's cookie jar.
const COOKIE_EXTRACTORS: &[&str] = &[
    "AuthUser",
    "ApiUser",
    "AnyUser",
    "OptionalUser",
    "AdminUser",
];

#[test]
fn no_cookie_extractor_is_named_under_the_api() {
    let sites = identifier_sites(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &["src/api/"],
        COOKIE_EXTRACTORS,
    );
    report("cookie-accepting extractors in API handler files", &sites);
}

#[tokio::test]
async fn a_session_cookie_authenticates_nothing_under_the_api_or_mcp() {
    let t = AppBuilder::new("route-inventory-cookie").build();
    let (_user, cookie) = signed_in(&t.store, "FlashTester", "flashtester@example.com");
    let mut honoured = Vec::new();
    for route in core_routes()
        .into_iter()
        .filter(|r| r.is_api() || r.path == "/mcp")
        .filter(|r| !listed(r, FRONT_DOOR))
    {
        let p = probe_as(&t.app, &route, OWN_ORIGIN, Some(&cookie), None).await;
        if p.status != axum::http::StatusCode::UNAUTHORIZED {
            honoured.push(p.line());
        }
    }
    report(
        "API or MCP routes that answered a session cookie with something other than 401",
        &honoured,
    );
}

#[test]
fn every_listed_exception_still_names_a_route() {
    let routes = core_routes();
    report(
        "front-door entries matching no route",
        &unmatched(FRONT_DOOR, &routes),
    );
    report(
        "protocol entries matching no route",
        &unmatched(PROTOCOL, &routes),
    );
}

#[test]
fn the_inventory_reads_multi_line_declarations_and_method_chains() {
    let routes = core_routes();
    let has =
        |method: &str, path: &str| routes.iter().any(|r| r.method == method && r.path == path);
    // Declared across several lines.
    assert!(has("POST", "/settings/password/set"));
    // `get(a).post(b)` on one path.
    assert!(has("GET", "/decks") && has("POST", "/decks"));
    // Mounted under the API prefix by module path.
    assert!(has("GET", "/api/v1/meta"));
    // The nested MCP service.
    assert!(has("POST", "/mcp"));
}
