//! What a client is told when its upload is bad: a sentence about the
//! file, never what zip, zstd, SQLite, serde or the filesystem said, and
//! never a path. Checked on both surfaces with hostile inputs, and pinned
//! at the source: no client-facing `Invalid` may interpolate a library
//! error.

use std::io::Write;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use flash_scan::workspace_crates;
use flash_server::testing::{api_bearer, send, signed_in, AppBuilder, BASE};

/// Words that belong to a library, an OS or a source tree, not to a
/// message for a person.
const FORBIDDEN: &[&str] = &[
    "os error",
    "sqlite",
    "rusqlite",
    "serde",
    "zip::",
    "zstd::",
    "panicked",
    "src/",
    "src\\",
    "c:\\",
    "/tmp/",
    "error code",
];

fn assert_clean(context: &str, body: &str) {
    let lower = body.to_lowercase();
    for word in FORBIDDEN {
        assert!(
            !lower.contains(word),
            "{context}: response leaks `{word}`:\n{body}"
        );
    }
}

fn multipart(
    path: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
    filename: &str,
    bytes: &[u8],
) -> Request<Body> {
    let boundary = "flashboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut req = Request::post(path).header(header::ORIGIN, BASE).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={boundary}"),
    );
    if let Some(cookie) = cookie {
        req = req.header(header::COOKIE, cookie);
    }
    if let Some(bearer) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    req.body(Body::from(body)).unwrap()
}

/// A real zip whose collection is not a database.
fn zip_with_garbage_collection() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        zip.start_file::<_, ()>("collection.anki2", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"this is not sqlite at all").unwrap();
        zip.finish().unwrap();
    }
    out.into_inner()
}

fn hostile_uploads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("garbage.apkg", b"\x00\x01\x02not a zip".to_vec()),
        (
            "truncated.apkg",
            b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec(),
        ),
        ("damaged.apkg", zip_with_garbage_collection()),
        ("bad-header.csv", b"\xff\xfe\x00\x00,\x00\n".to_vec()),
        ("empty.csv", b"".to_vec()),
    ]
}

#[tokio::test]
async fn hostile_uploads_get_a_sentence_not_a_stack_trace() {
    let t = AppBuilder::new("error-surface").build();
    let (user, cookie) = signed_in(&t.store, "FlashTester", "flashtester@example.com");
    let bearer = api_bearer(&t.store, user);

    for (name, bytes) in hostile_uploads() {
        let web = send(
            &t.app,
            multipart("/import/preview", Some(&cookie), None, name, &bytes),
        )
        .await;
        assert_eq!(web.status, StatusCode::BAD_REQUEST, "web {name}");
        assert_clean(&format!("web {name}"), &web.text());

        let api = send(
            &t.app,
            multipart("/api/v1/import/preview", None, Some(&bearer), name, &bytes),
        )
        .await;
        assert_eq!(api.status, StatusCode::BAD_REQUEST, "api {name}");
        assert_clean(&format!("api {name}"), &api.text());
        assert!(
            api.json()["error"]["message"].as_str().is_some(),
            "api {name}: {}",
            api.text()
        );
    }

    // A media upload of the wrong kind.
    let media = send(
        &t.app,
        multipart(
            "/media",
            Some(&cookie),
            None,
            "tool.exe",
            b"MZ\x90\x00garbage",
        ),
    )
    .await;
    assert!(media.status.is_client_error(), "{}", media.status);
    assert_clean("web media", &media.text());
}

/// The source of the rule: nowhere in the server or the hosted layer does
/// a client-facing `Invalid` carry a library error's text. The multipart
/// driver's own message is the one exception: it describes the client's
/// request, not our internals.
#[test]
fn no_client_facing_invalid_interpolates_a_library_error() {
    // Every way a library error's text could ride in an argument.
    const CARRIES_ERROR: &[&str] = &[
        "{e}",
        "{err}",
        "{e:",
        "{err:",
        "e.to_string()",
        "err.to_string()",
    ];
    // The multipart driver's message describes the client's request;
    // an over-cap error is the service's own sentence about the plan.
    // Compact text: no spaces between tokens.
    const ALLOWED: &[(&str, &str)] = &[
        ("invalid", "\"upload: {e}\""),
        ("invalid", "\"body: {e}\""),
        ("new", "\"over_cap\",err.to_string()"),
        ("invalid_params", "err.to_string(),None"),
    ];
    let mut offenders = Vec::new();
    let mut constructors = 0;
    for krate in workspace_crates() {
        for file in krate.source_files() {
            // Every constructor of a sentence the client sees, as a path
            // call or a method call, with its arguments as one text.
            let mut sites: Vec<(String, String, usize)> = Vec::new();
            for c in &file.calls {
                let client_facing = c.ends_with(&["Invalid"])
                    || c.ends_with(&["invalid"])
                    || c.ends_with(&["invalid_params"])
                    || c.ends_with(&["Failure", "user"])
                    || c.ends_with(&["ApiError", "new"]);
                if client_facing {
                    let name = c.path.last().cloned().unwrap_or_default();
                    sites.push((name, c.args.join(","), c.line));
                }
            }
            for m in &file.method_calls {
                if matches!(m.method.as_str(), "invalid" | "invalid_params") {
                    sites.push((m.method.clone(), m.args.join(","), m.line));
                }
            }
            for (name, args, line) in sites {
                constructors += 1;
                let carries = CARRIES_ERROR.iter().any(|p| args.contains(p));
                let allowed = ALLOWED
                    .iter()
                    .any(|(callee, fragment)| *callee == name && args.contains(fragment));
                if carries && !allowed {
                    offenders.push(format!("{}/{}:{line} {name}({args})", krate.name, file.rel));
                }
            }
        }
    }
    assert!(
        constructors > 50,
        "the scan found only {constructors} client-facing constructors"
    );
    assert!(
        offenders.is_empty(),
        "library errors interpolated into client-facing messages:\n{}",
        offenders.join("\n")
    );
}
