//! The parsers cap what they build, not only what they return: a file
//! past the row cap is refused before its rows are materialized, a raw
//! field is cut to what could survive into a card before any rewriter
//! sees it, a row's tags are clipped to what the store accepts, and a
//! file of nothing but bad rows reports the first hundred. Together
//! with the collection-size cap this bounds an import's memory and time
//! by the caps, not by the file.

use std::io::Write;

use flash_store::import::{
    parse_apkg, parse_csv, MAX_MESSAGES, MAX_PACKAGE_ENTRIES, MAX_RAW_FIELD_BYTES, MAX_ROWS,
};
use flash_store::{MAX_TAGS_PER_CARD, MAX_TAG_LEN};

const BASIC_MODEL: &str = r#"{"1":{"id":1,"name":"Basic","type":0,"flds":[{"name":"Front","ord":0},{"name":"Back","ord":1}],"tmpls":[{"name":"Card 1","ord":0,"qfmt":"{{Front}}","afmt":"{{FrontSide}}<hr id=answer>{{Back}}"}],"css":""}}"#;

/// A legacy package built straight into SQLite so the test does not pay
/// for the exporter: `models` is the col.models JSON, each note is
/// (id, tags, flds), each card is (id, nid, ord).
fn apkg(models: &str, notes: &[(i64, String, String)], cards: &[(i64, i64, i64)]) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!(
        "flash-caps-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("collection.anki2");
    {
        let mut conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE col (id INTEGER PRIMARY KEY, crt INTEGER, mod INTEGER, scm INTEGER,
                               ver INTEGER, dty INTEGER, usn INTEGER, ls INTEGER, conf TEXT,
                               models TEXT, decks TEXT, dconf TEXT, tags TEXT);
             CREATE TABLE notes (id INTEGER PRIMARY KEY, guid TEXT, mid INTEGER, mod INTEGER,
                                 usn INTEGER, tags TEXT, flds TEXT, sfld TEXT, csum INTEGER,
                                 flags INTEGER, data TEXT);
             CREATE TABLE cards (id INTEGER PRIMARY KEY, nid INTEGER, did INTEGER, ord INTEGER,
                                 mod INTEGER, usn INTEGER, type INTEGER, queue INTEGER, due INTEGER,
                                 ivl INTEGER, factor INTEGER, reps INTEGER, lapses INTEGER,
                                 left INTEGER, odue INTEGER, odid INTEGER, flags INTEGER, data TEXT);",
        )
        .unwrap();
        let decks = r#"{"1":{"id":1,"name":"Default"}}"#;
        conn.execute(
            "INSERT INTO col VALUES (1,0,0,0,11,0,0,0,'{}',?1,?2,'{}','')",
            [models, decks],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut note = tx
                .prepare("INSERT INTO notes VALUES (?1,'g',1,0,-1,?2,?3,'',0,0,'')")
                .unwrap();
            let mut card = tx
                .prepare("INSERT INTO cards VALUES (?1,?2,1,?3,0,-1,0,0,0,0,0,0,0,0,0,0,0,'')")
                .unwrap();
            for (id, tags, flds) in notes {
                note.execute(rusqlite::params![id, tags, flds]).unwrap();
            }
            for (id, nid, ord) in cards {
                card.execute(rusqlite::params![id, nid, ord]).unwrap();
            }
        }
        tx.commit().unwrap();
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

/// `notes` basic notes, one card each.
fn apkg_with_notes(notes: usize, field: &str, tags: &str) -> Vec<u8> {
    let notes: Vec<(i64, String, String)> = (1..=notes as i64)
        .map(|i| (i, tags.to_string(), format!("{field} {i}\u{1f}back {i}")))
        .collect();
    let cards: Vec<(i64, i64, i64)> = notes.iter().map(|(i, _, _)| (*i, *i, 0)).collect();
    apkg(BASIC_MODEL, &notes, &cards)
}

#[test]
fn a_package_past_the_row_cap_is_refused_before_its_rows_exist() {
    let pkg = apkg_with_notes(MAX_ROWS + 500, "front", "");
    let started = std::time::Instant::now();
    let err = parse_apkg(&pkg).unwrap_err();
    assert!(err.public().contains("more than 50000 cards"), "{err}");
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
}

#[test]
fn a_csv_past_the_row_cap_is_refused() {
    let mut csv = String::from("front,back\n");
    for i in 0..(MAX_ROWS + 10) {
        csv.push_str(&format!("q{i},a{i}\n"));
    }
    let err = parse_csv(csv.as_bytes()).unwrap_err();
    assert!(err.public().contains("more than 50000 cards"), "{err}");
}

#[test]
fn a_raw_field_is_clipped_before_the_rewriters_see_it() {
    // A megabyte of reveal-pattern triggers: the rewriters are not
    // linear, so an unclipped field would run for minutes. The length
    // assertion is the guarantee; the deadline only has to separate
    // "clipped" (seconds, even in a debug build on a slow runner) from
    // "unclipped" (minutes).
    let trigger = r#"<a onclick="document.getElementById('h').style.display='block'">x</a>"#;
    let field: String = std::iter::repeat_n(trigger, (1 << 20) / trigger.len()).collect();
    let pkg = apkg_with_notes(1, &field, "");
    let started = std::time::Instant::now();
    let parsed = parse_apkg(&pkg).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    assert!(
        parsed.rows[0]
            .front_html
            .as_ref()
            .is_none_or(|h| h.len() <= 40_000),
        "rich side bounded"
    );
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
}

#[test]
fn a_rows_tags_are_clipped_to_what_the_store_accepts() {
    let many: Vec<String> = (0..MAX_TAGS_PER_CARD + 20)
        .map(|i| format!("t{i}"))
        .collect();
    let long = "x".repeat(MAX_TAG_LEN + 1);
    let tags = format!("{} {long}", many.join(" "));
    let pkg = apkg_with_notes(1, "front", &tags);
    let parsed = parse_apkg(&pkg).unwrap();
    assert_eq!(parsed.rows[0].tags.len(), MAX_TAGS_PER_CARD);
    assert!(parsed.rows[0].tags.iter().all(|t| t.len() <= MAX_TAG_LEN));

    let csv = format!("front,back,tags\nq,a,\"{}\"\n", tags.replace(' ', ";"));
    let parsed = parse_csv(csv.as_bytes()).unwrap();
    assert_eq!(parsed.rows[0].tags.len(), MAX_TAGS_PER_CARD);
}

/// The 2026-09-06 review's confirmed panic: a manifest whose entry
/// length is a varint near `u64::MAX`, which an unchecked add wrapped
/// to an end before the start that passed the bounds check. Every
/// length the file chooses is now added checked; the manifest is simply
/// empty.
#[test]
fn a_manifest_length_that_would_wrap_is_not_a_panic() {
    // MediaEntries { entries: <len = u64::MAX - 6> ... }
    let mut manifest = vec![0x0A];
    let mut len = u64::MAX - 6;
    loop {
        let byte = (len & 0x7F) as u8;
        len >>= 7;
        if len == 0 {
            manifest.push(byte);
            break;
        }
        manifest.push(byte | 0x80);
    }
    manifest.extend_from_slice(&[0x0A, 0x01, b'x']);
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("meta", options).unwrap();
    out.write_all(&[0x08, 0x03]).unwrap();
    out.start_file("media", options).unwrap();
    out.write_all(&zstd::encode_all(&manifest[..], 1).unwrap())
        .unwrap();
    let bytes = out.finish().unwrap().into_inner();
    let entries = flash_store::import::read_media_manifest(&bytes).unwrap_or_default();
    assert!(entries.is_empty());
}

#[test]
fn a_csv_of_bad_rows_reports_the_first_hundred() {
    let mut csv = String::from("front,back\n");
    for _ in 0..(MAX_MESSAGES + 50) {
        csv.push_str(",\n");
    }
    csv.push_str("q,a\n");
    let parsed = parse_csv(csv.as_bytes()).unwrap();
    assert_eq!(parsed.messages.len(), MAX_MESSAGES);
    assert_eq!(parsed.skipped as usize, MAX_MESSAGES + 50);
    assert_eq!(parsed.rows.len(), 1);
}

/// The third review's High: the fields are clipped, but a card template
/// may name one of them any number of times, and the render is what the
/// pipelines see. A 1.6 KB package drove the parser to 14 GB. The render
/// is now capped like a field, and every pipeline clips on entry.
#[test]
fn a_template_cannot_multiply_a_field_past_the_raw_cap() {
    let qfmt = "{{Front}}".repeat(2_000);
    let models = BASIC_MODEL.replace(r#""qfmt":"{{Front}}""#, &format!(r#""qfmt":"{qfmt}""#));
    assert_ne!(models, BASIC_MODEL, "the template was substituted");
    let field = "<b>x</b> ".repeat(100 * 1024 / 9);
    let pkg = apkg(
        &models,
        &[(1, String::new(), format!("{field}\u{1f}back"))],
        &[(1, 1, 0)],
    );
    let started = std::time::Instant::now();
    let parsed = parse_apkg(&pkg).unwrap();
    assert_eq!(parsed.rows.len(), 1);
    assert!(parsed.rows[0].front.len() <= MAX_RAW_FIELD_BYTES);
    assert!(parsed.rows[0]
        .front_html
        .as_ref()
        .is_none_or(|h| h.len() <= 40_000));
    assert!(started.elapsed().as_secs() < 60, "{:?}", started.elapsed());
}

/// The row cap sits on the push, not on the note loop: one note whose
/// cards table holds more cards than the cap is the same sentence as
/// many notes, before the rows exist.
#[test]
fn one_note_with_many_cards_is_refused_at_the_row_cap() {
    let cards: Vec<(i64, i64, i64)> = (1..=(MAX_ROWS as i64 + 500)).map(|i| (i, 1, 0)).collect();
    let pkg = apkg(
        BASIC_MODEL,
        &[(1, String::new(), "front\u{1f}back".to_string())],
        &cards,
    );
    let started = std::time::Instant::now();
    let err = parse_apkg(&pkg).unwrap_err();
    assert!(err.public().contains("more than 50000 cards"), "{err}");
    assert!(started.elapsed().as_secs() < 30, "{:?}", started.elapsed());
}

/// A package's central directory is parsed in memory when it opens, so
/// the entry count is checked by the one opener before anything is
/// read from it: a million empty entries is a sentence, not a gigabyte.
#[test]
fn a_package_of_too_many_entries_is_refused_before_it_is_read() {
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..=MAX_PACKAGE_ENTRIES {
        out.start_file(i.to_string(), options).unwrap();
    }
    let bytes = out.finish().unwrap().into_inner();
    let err = parse_apkg(&bytes).unwrap_err();
    assert!(err.public().contains("more than 20000 files"), "{err}");
    let err = flash_store::import::read_media_manifest(&bytes).unwrap_err();
    assert!(err.public().contains("more than 20000 files"), "{err}");
}

/// The importer's shipped source, as the model reads it.
fn import_sources() -> Vec<flash_scan::SourceFile> {
    let out: Vec<flash_scan::SourceFile> =
        flash_scan::crate_at(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
            .source_files()
            .into_iter()
            .filter(|f| f.rel.starts_with("src/import/"))
            .collect();
    assert!(out.len() >= 10, "the import module was found");
    out
}

/// Every read of a table in the untrusted collection carries a LIMIT in
/// the same literal: a collection is bounded in bytes, and a table of
/// tiny rows would otherwise be bounded in nothing else. A literal built
/// with `format!` is a literal too.
#[test]
fn every_read_of_the_collection_carries_a_limit() {
    const TABLES: &[&str] = &[
        "notes",
        "cards",
        "revlog",
        "col",
        "decks",
        "notetypes",
        "fields",
        "templates",
        "deck_config",
    ];
    let mut unbounded = Vec::new();
    let mut reads = 0;
    for file in import_sources() {
        for literal in &file.string_literals {
            let sql = flash_scan::sql::normalize(&literal.value);
            let upper = sql.to_ascii_uppercase();
            if !flash_scan::sql::is_sql(&upper) {
                continue;
            }
            let anki = flash_scan::sql::tables_in(&upper)
                .into_iter()
                .any(|t| TABLES.iter().any(|a| a.eq_ignore_ascii_case(&t)));
            if !anki {
                continue;
            }
            reads += 1;
            if !upper.contains(" LIMIT ") {
                unbounded.push(format!("{}:{}: {sql}", file.rel, literal.line));
            }
        }
    }
    assert!(reads >= 8, "the scan found only {reads} collection reads");
    assert!(
        unbounded.is_empty(),
        "reads without a LIMIT:\n{}",
        unbounded.join("\n")
    );
}

/// Rows reach a `ParsedImport` through `push_row` alone, which carries
/// the cap; a parser that pushed directly could grow past it.
#[test]
fn every_parser_pushes_rows_through_the_cap() {
    let mut direct = Vec::new();
    for file in import_sources() {
        let pushes = file
            .method_calls
            .iter()
            .filter(|m| {
                m.method == "push" && (m.receiver == "rows" || m.receiver.ends_with(".rows"))
            })
            .count();
        let allowed = if file.rel == "src/import/mod.rs" {
            1
        } else {
            0
        };
        if pushes != allowed {
            direct.push(format!("{}: {pushes} direct push(es)", file.rel));
        }
    }
    assert!(direct.is_empty(), "{}", direct.join("\n"));
}
