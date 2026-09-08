//! Media guts: .apkg media manifests (legacy JSON and latest
//! zstd/protobuf), reference scanning during import, bomb-capped
//! extraction, and the media metadata repo.

use std::io::{Read, Write};

use flash_core::{MediaId, UserId};
use flash_store::export::{build_apkg, ExportCard};
use flash_store::import::{extract_media_file, parse_apkg, read_media_manifest};
use flash_store::media::MediaKind;
use flash_store::Store;

const NOW: i64 = 1_700_000_000_000;

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0, 1, 2, 3,
];
const MP3: &[u8] = &[b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0, 0, 0, 9, 9];

fn base_package() -> Vec<u8> {
    build_apkg(
        &[ExportCard {
            deck: "Pharm".into(),
            front: "placeholder".into(),
            back: "placeholder".into(),
            ..Default::default()
        }],
        &flash_store::export::ExportMedia::default(),
        NOW,
    )
    .unwrap()
}

/// Unpacks the collection db from an apkg, applies `sql`, and re-zips with
/// the given extra entries in place of the default empty media map.
fn rebuild(apkg: Vec<u8>, sql: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(apkg)).unwrap();
    let mut db_bytes = Vec::new();
    zip.by_name("collection.anki2")
        .unwrap()
        .read_to_end(&mut db_bytes)
        .unwrap();
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "flash-media-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("c.anki2");
    std::fs::write(&db_path, &db_bytes).unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(sql).unwrap();
    drop(conn);
    let db = std::fs::read(&db_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("collection.anki2", options).unwrap();
    out.write_all(&db).unwrap();
    for (name, bytes) in entries {
        out.start_file(*name, options).unwrap();
        out.write_all(bytes).unwrap();
    }
    out.finish().unwrap().into_inner()
}

fn set_fields_sql(front: &str, back: &str) -> String {
    format!(
        "UPDATE notes SET flds = '{}' || char(31) || '{}'",
        front.replace('\'', "''"),
        back.replace('\'', "''")
    )
}

// ---- legacy (JSON manifest) packages ----

#[test]
fn legacy_media_references_are_scanned_and_summarized() {
    let package = rebuild(
        base_package(),
        &set_fields_sql(
            r#"<img src="cat.jpg"> what animal?"#,
            "a cat [sound:meow.mp3]",
        ),
        &[
            (r#"media"#, br#"{"0": "cat.jpg", "1": "meow.mp3"}"#),
            ("0", PNG),
            ("1", MP3),
        ],
    );
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    let row = &parsed.rows[0];
    let names: Vec<&str> = row.media.iter().map(|r| r.filename.as_str()).collect();
    assert_eq!(names, vec!["cat.jpg", "meow.mp3"]);
    assert_eq!(row.media[0].kind, MediaKind::Image);
    assert_eq!(row.media[1].kind, MediaKind::Audio);

    // [sound:] tags are reference noise, not card text.
    assert_eq!(row.front, "what animal?");
    assert_eq!(row.back, "a cat");

    let m = &parsed.media;
    assert_eq!((m.images, m.audio, m.video, m.missing), (1, 1, 0, 0));
    assert_eq!(m.total_bytes, (PNG.len() + MP3.len()) as u64);
    assert!(
        parsed
            .messages
            .iter()
            .any(|msg| msg.contains("1 image, 1 audio clip")),
        "messages: {:?}",
        parsed.messages
    );
}

#[test]
fn legacy_media_extraction_round_trips() {
    let package = rebuild(
        base_package(),
        &set_fields_sql(r#"<img src="cat.jpg">x"#, "y"),
        &[(r#"media"#, br#"{"0": "cat.jpg"}"#), ("0", PNG)],
    );
    let manifest = read_media_manifest(&package).unwrap();
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].filename, "cat.jpg");
    assert_eq!(manifest[0].size, PNG.len() as u64);
    assert!(!manifest[0].zstd);
    let bytes = extract_media_file(&package, &manifest[0], 1024 * 1024).unwrap();
    assert_eq!(bytes, PNG);
    // Cap smaller than the file -> rejected, not truncated.
    assert!(extract_media_file(&package, &manifest[0], 4).is_err());
}

#[test]
fn referenced_but_absent_media_counts_as_missing() {
    let package = rebuild(
        base_package(),
        &set_fields_sql(r#"<img src="ghost.png">x"#, "y"),
        &[(r#"media"#, br#"{}"#)],
    );
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.media.images, 1);
    assert_eq!(parsed.media.missing, 1);
    assert_eq!(parsed.media.total_bytes, 0);
}

#[test]
fn media_only_sides_get_speakable_kind_labels() {
    // An audio-only front imports with a clean "[audio]" label, not the
    // raw [sound:...] filename.
    let package = rebuild(
        base_package(),
        &set_fields_sql("[sound:word.mp3]", "the answer"),
        &[(r#"media"#, br#"{"0": "word.mp3"}"#), ("0", MP3)],
    );
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    assert_eq!(parsed.rows[0].front, "[audio]");
    assert_eq!(parsed.rows[0].back, "the answer");

    // An image-only front imports too (it used to be skipped); the UI
    // shows the actual image via front_html.
    let package = rebuild(
        base_package(),
        &set_fields_sql(r#"<img src="cat.jpg">"#, "a cat"),
        &[(r#"media"#, br#"{"0": "cat.jpg"}"#), ("0", PNG)],
    );
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    assert_eq!(parsed.rows[0].front, "[image]");
    assert!(parsed.rows[0]
        .front_html
        .as_deref()
        .unwrap()
        .contains("media-ref"));

    // A genuinely blank side still skips.
    let package = rebuild(base_package(), &set_fields_sql("", "orphan back"), &[]);
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.rows.len(), 0);
    assert_eq!(parsed.skipped, 1);
}

#[test]
fn package_without_media_entry_has_empty_summary() {
    let parsed = parse_apkg(&base_package()).unwrap();
    assert_eq!(parsed.media.referenced(), 0);
    assert!(read_media_manifest(&base_package()).unwrap().is_empty());
}

// ---- latest (protobuf manifest, zstd) packages ----

fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn encode_media_entries(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, bytes) in files {
        let mut e = Vec::new();
        e.push(0x0A);
        e.push(name.len() as u8);
        e.extend_from_slice(name.as_bytes());
        e.push(0x10);
        encode_varint(bytes.len() as u64, &mut e);
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(bytes);
        e.push(0x1A);
        e.push(20);
        e.extend_from_slice(&hasher.digest().bytes());
        out.push(0x0A);
        encode_varint(e.len() as u64, &mut out);
        out.extend_from_slice(&e);
    }
    out
}

/// Builds a latest-generation package: meta v3, collection.anki21b and all
/// media entries zstd-compressed, manifest as zstd protobuf.
fn latest_package(front: &str, back: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    // Reuse the legacy build to get a valid collection db with our fields.
    let legacy = rebuild(base_package(), &set_fields_sql(front, back), &[]);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(legacy)).unwrap();
    let mut db = Vec::new();
    zip.by_name("collection.anki2")
        .unwrap()
        .read_to_end(&mut db)
        .unwrap();

    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("meta", options).unwrap();
    out.write_all(&[0x08, 0x03]).unwrap(); // PackageMetadata { version: 3 }
    out.start_file("collection.anki21b", options).unwrap();
    out.write_all(&zstd::encode_all(std::io::Cursor::new(db), 1).unwrap())
        .unwrap();
    out.start_file("media", options).unwrap();
    let manifest = encode_media_entries(files);
    out.write_all(&zstd::encode_all(std::io::Cursor::new(manifest), 1).unwrap())
        .unwrap();
    for (i, (_, bytes)) in files.iter().enumerate() {
        out.start_file(i.to_string(), options).unwrap();
        out.write_all(&zstd::encode_all(std::io::Cursor::new(*bytes), 1).unwrap())
            .unwrap();
    }
    out.finish().unwrap().into_inner()
}

#[test]
fn latest_package_manifest_and_extraction() {
    let package = latest_package(
        r#"<img src="cat.jpg">x"#,
        "y [sound:meow.mp3]",
        &[("cat.jpg", PNG), ("meow.mp3", MP3)],
    );
    let manifest = read_media_manifest(&package).unwrap();
    assert_eq!(manifest.len(), 2);
    assert_eq!(manifest[0].filename, "cat.jpg");
    assert_eq!(manifest[0].zip_name, "0");
    assert_eq!(manifest[0].size, PNG.len() as u64);
    assert!(manifest[0].zstd);
    assert!(manifest[0].sha1.is_some());

    let bytes = extract_media_file(&package, &manifest[0], 1024 * 1024).unwrap();
    assert_eq!(bytes, PNG);
    let bytes = extract_media_file(&package, &manifest[1], 1024 * 1024).unwrap();
    assert_eq!(bytes, MP3);

    // The notes themselves still parse, with refs attached.
    let parsed = parse_apkg(&package).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    assert_eq!(parsed.rows[0].media.len(), 2);
    assert_eq!(parsed.media.total_bytes, (PNG.len() + MP3.len()) as u64);
}

#[test]
fn latest_package_sha1_mismatch_is_rejected() {
    let package = latest_package(r#"<img src="cat.jpg">x"#, "y", &[("cat.jpg", PNG)]);
    let mut manifest = read_media_manifest(&package).unwrap();
    manifest[0].sha1 = Some([0u8; 20]);
    let err = extract_media_file(&package, &manifest[0], 1024 * 1024).unwrap_err();
    assert!(err.public().contains("hash"), "{err}");
}

// ---- media metadata repo ----

fn store_with_user() -> (Store, UserId) {
    let store = Store::open_in_memory().unwrap();
    let user = store.create_user("M", None, "member", NOW).unwrap();
    (store, user)
}

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn media_rows_dedupe_and_scope_to_owner() {
    let (store, user) = store_with_user();
    let other = store.create_user("O", None, "member", NOW).unwrap();

    let id = store
        .create_media(
            user,
            SHA_A,
            "cat.jpg",
            "image/jpeg",
            MediaKind::Image,
            100,
            NOW,
        )
        .unwrap();
    let again = store
        .create_media(
            user,
            SHA_A,
            "cat.jpg",
            "image/jpeg",
            MediaKind::Image,
            100,
            NOW,
        )
        .unwrap();
    assert_eq!(id, again, "same file twice = same row");

    let row = store.get_media(user, id).unwrap().unwrap();
    assert_eq!(row.filename, "cat.jpg");
    assert_eq!(row.kind, MediaKind::Image);
    assert_eq!(row.size, 100);

    // Someone else's id resolves to nothing.
    assert!(store.get_media(other, id).unwrap().is_none());
    assert_eq!(store.media_bytes_used(user).unwrap(), 100);
    assert_eq!(store.media_bytes_used(other).unwrap(), 0);
}

#[test]
fn media_quota_sums_and_delete_reports_remaining_refs() {
    let (store, user) = store_with_user();
    let other = store.create_user("O", None, "member", NOW).unwrap();
    let a = store
        .create_media(user, SHA_A, "a.png", "image/png", MediaKind::Image, 70, NOW)
        .unwrap();
    store
        .create_media(
            user,
            SHA_B,
            "b.mp3",
            "audio/mpeg",
            MediaKind::Audio,
            30,
            NOW,
        )
        .unwrap();
    assert_eq!(store.media_bytes_used(user).unwrap(), 100);

    // The other user references the same blob (shared-deck dedupe).
    let other_a = store
        .create_media(
            other,
            SHA_A,
            "a.png",
            "image/png",
            MediaKind::Image,
            70,
            NOW,
        )
        .unwrap();

    // Deleting user's row leaves the other reference: blob must stay.
    assert_eq!(store.delete_media(user, a).unwrap(), 1);
    assert_eq!(store.media_bytes_used(user).unwrap(), 30);
    // Last reference gone: blob is now removable.
    assert_eq!(store.delete_media(other, other_a).unwrap(), 0);
    // Deleting something not yours fails.
    assert!(store.delete_media(other, MediaId(999)).is_err());
}

#[test]
fn card_media_links_enforce_ownership() {
    let (store, user) = store_with_user();
    let other = store.create_user("O", None, "member", NOW).unwrap();
    let deck = store.create_deck(user, "D", "", NOW).unwrap();
    let cards = store
        .create_cards(
            user,
            deck,
            &[(
                flash_core::validate_card_text("f", "b").unwrap(),
                Vec::new(),
            )],
            None,
            NOW,
        )
        .unwrap();
    let media = store
        .create_media(user, SHA_A, "a.png", "image/png", MediaKind::Image, 10, NOW)
        .unwrap();
    store.link_card_media(user, cards[0], media).unwrap();
    // Re-linking is a no-op, not an error.
    store.link_card_media(user, cards[0], media).unwrap();
    // Another user can't link to media they don't own.
    assert!(store.link_card_media(other, cards[0], media).is_err());
}
