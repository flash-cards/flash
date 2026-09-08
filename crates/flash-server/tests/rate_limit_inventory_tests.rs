//! Every mutation the server mounts sits behind a per-address budget.
//! The inventory comes from the source tree (see `testing::routes`), the
//! limiters are set to a burst of `TIGHT_BURST`, and each mutating route
//! is sent one request more than that as a stranger: the last must be a
//! 429. A route that is not is a route whoever wrote it forgot to put
//! under a band, and the build says so. The limiters run before
//! authentication, so a stranger's probe proves the same thing a
//! member's would.
//!
//! Reads are not probed: pages, assets and partials are cheap and the
//! heavy reads (export, stats) hold a per-user permit.

use std::path::Path;

use axum::http::StatusCode;
use flash_server::testing::routes::{inventory, probe, Route, OWN_ORIGIN};
use flash_server::testing::{AppBuilder, TIGHT_BURST, TIGHT_BURST_GENEROUS};

/// The mutations that must sit behind the strict band (a small burst,
/// keyed by address and route): anything that spends a secret, mints
/// one, or sends mail. Every other mutation sits behind the generous
/// per-address budget. A route moving from one band to the other is a
/// change to this list, not a silent one.
const STRICT_BAND: &[(&str, &str)] = &[
    // Browser sign-in, enrollment, reset, sign-out.
    ("POST", "/auth/login/password"),
    ("POST", "/auth/login/start"),
    ("POST", "/auth/login/finish"),
    ("POST", "/auth/logout"),
    ("POST", "/auth/enroll/{token}/password"),
    ("POST", "/auth/enroll/{token}/start"),
    ("POST", "/auth/enroll/finish"),
    ("POST", "/reset"),
    ("POST", "/auth/reset/{token}"),
    // OAuth: registration, consent, token minting.
    ("POST", "/oauth/register"),
    ("POST", "/oauth/consent"),
    ("POST", "/oauth/token"),
    // The app's sign-in, enrollment, reset and refresh. Its sign-out
    // spends a bearer it already holds and sits in the generous band.
    ("POST", "/api/v1/auth/password"),
    ("POST", "/api/v1/auth/passkey/login/start"),
    ("POST", "/api/v1/auth/passkey/login/finish"),
    ("POST", "/api/v1/auth/refresh"),
    ("POST", "/api/v1/auth/enroll/{token}/password"),
    ("POST", "/api/v1/auth/enroll/{token}/passkey/start"),
    ("POST", "/api/v1/auth/enroll/{token}/passkey/finish"),
    ("POST", "/api/v1/auth/reset"),
    ("POST", "/api/v1/auth/reset/{token}"),
];

fn mutations() -> Vec<Route> {
    let routes: Vec<Route> = inventory(&[Path::new(env!("CARGO_MANIFEST_DIR"))])
        .into_iter()
        .filter(Route::is_mutation)
        .collect();
    assert!(
        routes.len() > 40,
        "inventory found {} mutations",
        routes.len()
    );
    routes
}

#[tokio::test]
async fn every_mutation_is_rate_limited() {
    let mut unlimited = Vec::new();
    let mut strict = Vec::new();
    let mut generous = Vec::new();
    for route in mutations() {
        // A fresh app per route: one shared bucket in the harness, so
        // the count is exactly this route's. The request that draws
        // the 429 says which band answered.
        let t = AppBuilder::new("rate-limit-inventory")
            .tight_rate_limits()
            .build();
        let mut refused_at = None;
        for n in 1..=TIGHT_BURST_GENEROUS + 1 {
            let status = probe(&t.app, &route, OWN_ORIGIN).await.status;
            if status == StatusCode::TOO_MANY_REQUESTS {
                refused_at = Some(n);
                break;
            }
        }
        let key = (route.method.clone(), route.path.clone());
        match refused_at {
            Some(n) if n == TIGHT_BURST + 1 => strict.push(key),
            Some(n) if n == TIGHT_BURST_GENEROUS + 1 => generous.push(key),
            Some(n) => unlimited.push(format!(
                "{:<6} {:<48} -> 429 after {n} requests, which is neither band  [{}]",
                route.method, route.path, route.site
            )),
            None => unlimited.push(format!(
                "{:<6} {:<48} -> no 429 after {} requests  [{}]",
                route.method,
                route.path,
                TIGHT_BURST_GENEROUS + 1,
                route.site
            )),
        }
    }
    assert!(
        unlimited.is_empty(),
        "mutations that are not behind a rate-limit band:\n{}",
        unlimited.join("\n")
    );
    // The strict set is exactly the listed one, in both directions.
    let listed = |(m, p): &(String, String)| STRICT_BAND.iter().any(|(lm, lp)| lm == m && lp == p);
    let demoted: Vec<String> = strict
        .iter()
        .filter(|k| !listed(k))
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    let promoted: Vec<String> = generous
        .iter()
        .filter(|k| listed(k))
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    let missing: Vec<String> = STRICT_BAND
        .iter()
        .filter(|(m, p)| !strict.iter().any(|(sm, sp)| sm == m && sp == p))
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    assert!(
        demoted.is_empty() && promoted.is_empty() && missing.is_empty(),
        "strict band drift.\nstrict but not listed: {demoted:?}\nlisted but generous: {promoted:?}\nlisted but not found strict: {missing:?}\nall strict: {:?}",
        strict.iter().map(|(m, p)| format!("{m} {p}")).collect::<Vec<_>>()
    );
}

/// The probe is not vacuous: with the normal limits the same requests
/// are not throttled, so a 429 above really came from the tight limiter
/// and not from the route refusing the stranger some other way.
#[tokio::test]
async fn the_normal_limits_do_not_throttle_a_handful_of_requests() {
    let t = AppBuilder::new("rate-limit-inventory-normal").build();
    let route = mutations()
        .into_iter()
        .find(|r| r.path == "/decks" && r.method == "POST")
        .expect("POST /decks in the inventory");
    for _ in 0..=TIGHT_BURST {
        let status = probe(&t.app, &route, OWN_ORIGIN).await.status;
        assert_ne!(status, StatusCode::TOO_MANY_REQUESTS);
    }
}
