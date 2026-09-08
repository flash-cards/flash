//! Inputs that used to panic (and, with `panic = "abort"` in release,
//! took the whole process down). Each case reaches its function through
//! the public surface a user can drive: an upload's filename, a field of
//! an imported deck, the editor's cloze parser. They must all return.

use flash_store::import::{parse_css_class_colors, remap_colors};
use flash_store::media::{sanitize_filename, scan_refs};
use flash_store::notes::{GeneratedCard, NoteType, MAX_FIELD_HTML};
use flash_store::richtext::{sanitize_with_media, strip_leading_text_html};

/// Hostile fields as the editor would hand them over: sanitized first.
fn generate_cards(kind: NoteType, front: &str, back: &str) -> Result<Vec<GeneratedCard>, String> {
    flash_store::notes::generate_cards(
        kind,
        &sanitize_with_media(front),
        &sanitize_with_media(back),
    )
}

#[test]
fn filename_cap_lands_inside_a_multibyte_character() {
    // 1 + 60×2 = 121 bytes; byte 120 is the middle of an 'é'.
    let name = format!("a{}.png", "é".repeat(60));
    let out = sanitize_filename(&name);
    assert!(out.chars().count() <= 120);
    assert!(out.starts_with("aé"));
    // And a name that is nothing but multibyte characters past the cap.
    let long = "日本語".repeat(100);
    assert_eq!(sanitize_filename(&long).chars().count(), 120);
}

#[test]
fn percent_sequences_followed_by_multibyte_do_not_panic() {
    for html in [
        r#"<img src="x%aé.png">"#,
        r#"<img src="x%zé.png">"#,
        "[sound:%aé]",
        "[sound:%é]",
        r#"<img src="%">"#,
        r#"<img src="%%%">"#,
        r#"<img src="ok%20name.png">"#,
    ] {
        let refs = scan_refs(html);
        // The last one decodes normally; the rest just must not crash.
        if html.contains("%20") {
            assert_eq!(refs[0].filename, "ok name.png");
        }
    }
}

#[test]
fn entity_window_on_a_multibyte_boundary_does_not_panic() {
    // "&amp;" + "éé…" puts byte 8 inside a character.
    let html = format!("Q &amp;{}<hr>A", "é".repeat(20));
    let _ = strip_leading_text_html(&html, &format!("Q &{}", "é".repeat(20)));
    let _ = strip_leading_text_html("&éééééééé", "&");
    let _ = strip_leading_text_html("&amp;", "&");
    assert_eq!(
        strip_leading_text_html("Q&amp;A<hr>rest", "Q&A").as_deref(),
        Some("<hr>rest")
    );
}

#[test]
fn non_ascii_colour_values_do_not_panic() {
    let map = parse_css_class_colors(".x{color:#aéééb} .y{color:#ééé}");
    for html in [
        r##"<span style="color:#aéééb">x</span>"##,
        r##"<font color="#ééé">x</font>"##,
        r##"<font color="#éééééé">x</font>"##,
        r##"<span class="x">x</span>"##,
    ] {
        let _ = remap_colors(html, &map);
    }
}

#[test]
fn deeply_nested_cloze_parses_without_overflowing_the_stack() {
    // As deep as the editor's field cap allows: each level is 6 bytes to
    // open and 2 to close.
    let levels = (MAX_FIELD_HTML - 16) / 8;
    let mut s = String::new();
    for _ in 0..levels {
        s.push_str("{{c1::");
    }
    s.push('x');
    for _ in 0..levels {
        s.push_str("}}");
    }
    assert!(s.len() <= MAX_FIELD_HTML);
    let cards = generate_cards(NoteType::Cloze, &s, "").expect("parses");
    assert!(!cards.is_empty());
}

/// A minimal .apkg around a caller-built SQLite file.
fn apkg_with(build: impl FnOnce(&rusqlite::Connection)) -> Vec<u8> {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!(
        "flash-hostile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("collection.anki2");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        build(&conn);
    }
    let bytes = std::fs::read(&db).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("collection.anki2", options).unwrap();
    out.write_all(&bytes).unwrap();
    out.start_file("media", options).unwrap();
    out.write_all(b"{}").unwrap();
    out.finish().unwrap().into_inner()
}

#[test]
fn a_collection_whose_tables_are_views_is_refused() {
    // A VIEW named `notes` runs whatever SQL its author wrote on every
    // SELECT: a recursive CTE that never ends, for instance. Only real
    // tables are read.
    let pkg = apkg_with(|conn| {
        conn.execute_batch(
            "CREATE TABLE col (id INTEGER PRIMARY KEY, crt INTEGER, mod INTEGER, scm INTEGER,
                               ver INTEGER, dty INTEGER, usn INTEGER, ls INTEGER, conf TEXT,
                               models TEXT, decks TEXT, dconf TEXT, tags TEXT);
             INSERT INTO col VALUES (1,0,0,0,11,0,0,0,'{}','{}','{}','{}','');
             CREATE VIEW notes AS
               WITH RECURSIVE forever(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM forever)
               SELECT n AS id, '' AS guid, 0 AS mid, 0 AS mod, -1 AS usn, '' AS tags,
                      'q' || char(31) || 'a' AS flds, 'q' AS sfld, 0 AS csum, 0 AS flags,
                      '' AS data FROM forever;
             CREATE TABLE cards (id INTEGER PRIMARY KEY, nid INTEGER, did INTEGER, ord INTEGER,
                                 mod INTEGER, usn INTEGER, type INTEGER, queue INTEGER, due INTEGER,
                                 ivl INTEGER, factor INTEGER, reps INTEGER, lapses INTEGER,
                                 left INTEGER, odue INTEGER, odid INTEGER, flags INTEGER, data TEXT);",
        )
        .unwrap();
    });
    let started = std::time::Instant::now();
    let err = flash_store::import::parse_apkg(&pkg).unwrap_err();
    // The uploader learns the shape of the problem; the attacker-chosen
    // object names and SQLite's words stay in the log.
    assert!(err.public().contains("unexpected layout"), "{err}");
    assert!(err.detail().contains("notes"), "{}", err.detail());
    for word in ["sqlite", "not tables", "os error"] {
        assert!(!err.public().to_lowercase().contains(word), "{err}");
    }
    assert!(started.elapsed().as_secs() < 5);
}

/// The parser is one pass whatever the field contains: a field of nothing
/// but openers, one that closes only at the very end, and one whose
/// openers sit inside a hint all cost the same as reading the field.
#[test]
fn many_unterminated_cloze_openers_finish_quickly() {
    let openers = "{{c1::".repeat(MAX_FIELD_HTML / 6);
    let closed_once = format!("{}x}}}}", "{{c1::".repeat(MAX_FIELD_HTML / 6 - 1));
    let in_hint = format!("{{{{c1::a::{}", "{{c2::".repeat(MAX_FIELD_HTML / 6 - 2));
    for field in [openers, closed_once, in_hint] {
        let started = std::time::Instant::now();
        let _ = generate_cards(NoteType::Cloze, &field, "");
        assert!(
            started.elapsed().as_millis() < 500,
            "{:?} for {} bytes",
            started.elapsed(),
            field.len()
        );
    }
}

/// The 2026-09-06 review's sanitizer bypass. A `class=` smuggled inside
/// another attribute's quoted value survives ammonia (it is just text
/// in that value); the "clean look" rewriter used to find it with a
/// substring scan and re-emit it as a real class, then store the result
/// as-is. The rewriter now walks attributes and re-sanitizes, and the
/// store takes only `SanitizedHtml`, so neither half can recur.
#[test]
fn a_class_smuggled_inside_another_attribute_does_not_survive_the_clean_look() {
    let field = r#"<span style="color:red">x</span><span data-media="x class='fl-dialog-overlay editor-error hl-red'" data-kind="file" class="media-ref">Session expired</span>"#;
    let stored = flash_store::richtext::sanitize_card_html(field);
    let cleaned = flash_store::richtext::strip_highlight_classes(&stored);
    // The smuggled words may survive as inert text inside the data
    // attribute; what matters is that no class attribute carries them.
    assert!(
        !cleaned.contains(r#"class="fl-dialog-overlay"#)
            && !cleaned.contains("editor-error\"")
            && cleaned.contains(r#"class="media-ref""#),
        "{cleaned}"
    );
    // And a genuine highlight class is still stripped.
    let highlighted =
        flash_store::richtext::sanitize_card_html(r#"<span class="hl-red cloze-answer">a</span>"#);
    assert_eq!(
        flash_store::richtext::strip_highlight_classes(&highlighted),
        r#"<span class="cloze-answer">a</span>"#
    );
}

/// A parked import preview is read back through serde; the typed field
/// re-sanitizes on the way in, so even a rewritten file on disk cannot
/// put markup past the sanitizer.
#[test]
fn deserializing_rich_html_sanitizes_it() {
    let json = r#"{"front":"q","back":"a","front_html":"<script>alert(1)</script><b>y</b>","back_html":null,"tags":[],"deck":null}"#;
    let row: flash_store::import::ImportRow = serde_json::from_str(json).unwrap();
    assert_eq!(row.front_html.as_deref(), Some("<b>y</b>"));
}
