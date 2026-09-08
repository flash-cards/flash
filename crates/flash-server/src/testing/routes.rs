//! The route probe: one anonymous request to each route of the
//! inventory, and what came back. The inventory itself is the source
//! model's (`flash_scan::routes`): every `.route("…", method(…))` in the
//! scanned crates, so a route added anywhere is probed on the next run,
//! and it either turns a stranger away or has to be named on a public
//! list with a reason.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;

pub use flash_scan::routes::{identifier_sites, inventory, Route};

use super::{send, BASE};

/// The route's path with every placeholder filled by a plausible value.
pub fn concrete_path(route: &Route) -> String {
    let mut out = String::new();
    let mut rest = route.path.as_str();
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('}').expect("closing brace in route path");
        let name = after[..end].trim_start_matches('*');
        out.push_str(match name {
            "token" => "sometoken",
            "slug" => "someslug",
            "file" => "x.png",
            "variant" => "monthly",
            "decision" => "approve",
            _ => "1",
        });
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// What one anonymous request to a route came back with.
#[derive(Debug)]
pub struct Probe {
    pub route: Route,
    pub status: StatusCode,
    pub location: String,
    pub body: String,
}

impl Probe {
    /// The browser's "sign in first" answer (the login redirect, with or
    /// without a return path) or the API's 401. A redirect to the login
    /// page carrying an error is an answer, not a demand.
    pub fn turned_away(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
            || (self.status.is_redirection()
                && (self.location == "/login" || self.location.starts_with("/login?next=")))
    }

    /// The same-origin guard's refusal.
    pub fn refused_cross_origin(&self) -> bool {
        self.status == StatusCode::FORBIDDEN && self.body.contains("cross-origin")
    }

    pub fn line(&self) -> String {
        format!(
            "{:<6} {:<48} -> {} {}  [{}]",
            self.route.method,
            self.route.path,
            self.status.as_u16(),
            self.location,
            self.route.site
        )
    }
}

/// Sends the route one request with no cookie and no bearer, from
/// `origin` (the site's own origin unless a test asks for a foreign one),
/// with a browser User-Agent so bot heuristics don't answer first.
pub async fn probe(app: &Router, route: &Route, origin: &str) -> Probe {
    probe_as(app, route, origin, None, None).await
}

/// `probe` with a credential attached: a session cookie, a bearer, or
/// both, for checks of which credential a band honours.
pub async fn probe_as(
    app: &Router,
    route: &Route,
    origin: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
) -> Probe {
    let method = Method::from_bytes(route.method.as_bytes()).unwrap();
    let mut req = Request::builder()
        .method(method)
        .uri(concrete_path(route))
        .header(header::USER_AGENT, "Mozilla/5.0 (route inventory)")
        .header(header::ACCEPT, "*/*");
    if route.is_mutation() {
        req = req
            .header(header::ORIGIN, origin)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    if let Some(bearer) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let reply = send(app, req.body(Body::empty()).unwrap()).await;
    Probe {
        route: route.clone(),
        status: reply.status,
        location: reply.location().to_string(),
        body: reply.text(),
    }
}

/// Whether a route is on a list of `(METHOD, path)` entries; a path
/// ending in `/*` matches every route under that prefix.
pub fn listed(route: &Route, list: &[(&str, &str)]) -> bool {
    list.iter().any(|(method, path)| {
        *method == route.method
            && match path.strip_suffix("/*") {
                Some(prefix) => route.path.starts_with(prefix),
                None => *path == route.path,
            }
    })
}

/// List entries that match no route in the inventory: a route that was
/// renamed or removed leaves a stale exception behind otherwise.
pub fn unmatched<'a>(list: &'a [(&'a str, &'a str)], routes: &[Route]) -> Vec<String> {
    list.iter()
        .filter(|(method, path)| !routes.iter().any(|route| listed(route, &[(method, path)])))
        .map(|(method, path)| format!("{method} {path}"))
        .collect()
}

/// The same origin every test config runs under.
pub const OWN_ORIGIN: &str = BASE;
/// A site that is not us.
pub const FOREIGN_ORIGIN: &str = "https://attacker.example";
