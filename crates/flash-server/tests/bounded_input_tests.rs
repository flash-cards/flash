//! Every string and list a request can carry is bounded before a handler
//! sees it. The limits live in `flash_server::bounds`; each request
//! struct's field names its bound with `deserialize_with`, and this scan
//! walks every `Deserialize` type in every crate's shipped source and
//! fails the build for a field of an unbounded type that names none. A
//! type that is not a request (a file the server wrote itself, a
//! parser's output that its own caps bound) is listed below with the
//! reason, and the list is checked against the scan so it cannot rot.
//!
//! What this buys: a new endpoint cannot accept an unbounded field by
//! omission, and the number that bounds it is in one table rather than
//! wherever someone remembered to check.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use flash_scan::{crate_at, workspace_crates, Field, TypeDecl};
use flash_server::bounds;
use flash_server::testing::{send, AppBuilder};

/// Types whose unbounded fields are not request input, by name, with
/// the reason. A name that no crate in the tree declares is simply not
/// checked (a downstream extension's types are listed here too, for the
/// tree that has them).
const NOT_A_REQUEST: &[(&str, &str)] = &[
    (
        "PendingImport",
        "the parked preview the server wrote itself; its rows come from the parser, whose caps bound them",
    ),
    (
        "ImportRow",
        "the parser's output for one card; the parser caps rows and field sizes, and the rich sides are SanitizedHtml",
    ),
    (
        "MediaRef",
        "one media reference the parser found in a field; filenames are sanitized and capped by the media module",
    ),
    (
        "ParsedImport",
        "a parser's whole result (rows and messages), built from a file the parser itself caps and parked by the server",
    ),
    (
        "ImportedSettings",
        "FSRS parameters read from a package's deck options; validated by building a scheduler before use",
    ),
    (
        "DeckPayload",
        "a share's frozen snapshot, read back from a column the server wrote itself; its cards came from the store",
    ),
    (
        "SharedCard",
        "one card of a share snapshot; the sides and tags were the store's own rows, and the HTML was sanitized when written",
    ),
    (
        "MediaMeta",
        "one media reference of a share snapshot, copied from a media row the media module had already capped",
    ),
];

/// A type that carries unbounded bytes: a string, a list, a map, a
/// document, or an owned buffer, anywhere in the field's type.
const UNBOUNDED: &[&str] = &[
    "String", "str", "Vec", "HashMap", "BTreeMap", "HashSet", "Value", "Cow", "Box", "Bytes",
];

fn derives_deserialize(t: &TypeDecl) -> bool {
    t.derives.iter().any(|d| d == "Deserialize")
}

/// The fields of one declaration whose type is unbounded and whose
/// attributes name no `bounds::` deserializer.
fn unbounded_fields(t: &TypeDecl) -> Vec<&Field> {
    t.fields
        .iter()
        .filter(|f| f.ty.idents.iter().any(|i| UNBOUNDED.contains(&i.as_str())))
        .filter(|f| {
            !f.deserialize_with
                .as_deref()
                .is_some_and(|d| d.contains("bounds::"))
        })
        .collect()
}

fn field_label(f: &Field) -> String {
    match (&f.variant, &f.name) {
        (Some(v), Some(n)) => format!("{v}.{n}: {}", f.ty.compact),
        (Some(v), None) => format!("{v}({})", f.ty.compact),
        (None, Some(n)) => format!("{n}: {}", f.ty.compact),
        (None, None) => f.ty.compact.clone(),
    }
}

#[test]
fn every_request_string_and_list_names_its_bound() {
    let mut offenders = Vec::new();
    let mut listed_seen: Vec<String> = Vec::new();
    let mut declared_names: Vec<String> = Vec::new();
    for krate in workspace_crates() {
        for file in krate.source_files() {
            // A renamed derive would hide a request type from the scan.
            for u in &file.uses {
                if u.path.last().is_some_and(|l| l == "Deserialize") && u.rename.is_some() {
                    panic!(
                        "{}/{}:{}: a renamed Deserialize import hides derives from the scan",
                        krate.name, file.rel, u.line
                    );
                }
            }
            for declared in file.types.iter().filter(|t| derives_deserialize(t)) {
                declared_names.push(declared.name.clone());
                let fields = unbounded_fields(declared);
                if fields.is_empty() {
                    continue;
                }
                if NOT_A_REQUEST.iter().any(|(t, _)| *t == declared.name) {
                    listed_seen.push(declared.name.clone());
                    continue;
                }
                for field in fields {
                    offenders.push(format!(
                        "{}/{}:{}: {} `{}`",
                        krate.name,
                        file.rel,
                        field.line,
                        declared.name,
                        field_label(field)
                    ));
                }
            }
        }
    }
    // An entry is stale when its type is declared in this tree and no
    // longer has an unbounded field; a type no crate here declares
    // belongs to a crate that is not checked out.
    let stale: Vec<String> = NOT_A_REQUEST
        .iter()
        .filter(|(t, _)| {
            declared_names.iter().any(|d| d == t) && !listed_seen.contains(&t.to_string())
        })
        .map(|(t, _)| t.to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "request fields with no bound (add `#[serde(deserialize_with = \"crate::bounds::…\")]`, \
         or list the type in NOT_A_REQUEST with a reason):\n{}",
        offenders.join("\n")
    );
    assert!(
        stale.is_empty(),
        "NOT_A_REQUEST names types the scan no longer finds unbounded: {}",
        stale.join(", ")
    );
}

/// The scan reads real declarations: it must find the server's own
/// request structs, or a broken parser would pass vacuously.
#[test]
fn the_scan_sees_the_request_structs() {
    let mut names = Vec::new();
    for file in crate_at(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).source_files() {
        names.extend(
            file.types
                .iter()
                .filter(|t| derives_deserialize(t))
                .map(|t| t.name.clone()),
        );
    }
    for expected in [
        "QuickAddBody",
        "AuthorizeParams",
        "CreateCardsArgs",
        "NoteForm",
    ] {
        assert!(names.contains(&expected.to_string()), "{expected} not seen");
    }
}

/// Request data that never passes through a `Deserialize` struct. Path
/// segments and raw queries are bounded for every handler at once by
/// `middleware::uri_length` (probed below). A multipart body is not:
/// each handler reading one must name a bound in its own body.
#[test]
fn every_handler_reading_a_multipart_body_names_a_bound() {
    let mut offenders = Vec::new();
    let mut readers = 0;
    for krate in workspace_crates() {
        for file in krate.source_files() {
            // A function names a bound when a `bounds::…` path sits in its
            // body; a thin wrapper that hands the body to such a function
            // in the same file is bounded by it.
            let names_bound = |name: &str| {
                file.paths.iter().any(|p| {
                    p.function.as_deref() == Some(name) && p.segments.iter().any(|s| s == "bounds")
                })
            };
            let delegates_to_bounded = |name: &str| {
                file.calls
                    .iter()
                    .filter(|c| c.function.as_deref() == Some(name))
                    .filter_map(|c| c.path.last())
                    .any(|callee| file.function(callee).is_some() && names_bound(callee))
            };
            for function in &file.functions {
                let reads = function
                    .params
                    .iter()
                    .any(|p| p.ty.idents.iter().any(|i| i == "Multipart"));
                if !reads {
                    continue;
                }
                readers += 1;
                if !names_bound(&function.name) && !delegates_to_bounded(&function.name) {
                    offenders.push(format!("{}/{}: {}", krate.name, file.rel, function.name));
                }
            }
        }
    }
    assert!(readers >= 2, "the scan found {readers} multipart readers");
    assert!(
        offenders.is_empty(),
        "handlers reading a multipart body with no bound in the body:\n{}",
        offenders.join("\n")
    );
}

/// The bound on every path segment and query string: a URL past
/// `bounds::URI` is refused before any handler sees it.
#[tokio::test]
async fn a_url_past_the_cap_is_refused_before_routing() {
    let t = AppBuilder::new("bounded-uri").build();
    let long = "a".repeat(bounds::URI + 1);
    for uri in [format!("/{long}"), format!("/?{long}")] {
        let res = send(&t.app, Request::get(&uri).body(Body::empty()).unwrap()).await;
        assert_eq!(res.status, StatusCode::URI_TOO_LONG, "{}", &uri[..20]);
    }
    let fine = "a".repeat(bounds::URI - 2);
    let res = send(
        &t.app,
        Request::get(format!("/{fine}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_ne!(res.status, StatusCode::URI_TOO_LONG);
}
