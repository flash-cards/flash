//! Full-fidelity import integration: cloze expansion, template rendering,
//! reversed/suspended cards, rich-HTML sanitization, LaTeX conversion,
//! deck-options extraction, and the cloze round trip back out through the
//! exporter — all through the real parse_apkg path.

use std::io::{Read, Write};

use flash_store::export::{build_apkg, ExportCard};
use flash_store::import::parse_apkg;

const NOW: i64 = 1_700_000_000_000;

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

/// Note id / card id our exporter assigns to the single base card.
const NOTE_ID: i64 = NOW;
const CARD_ID: i64 = NOW + 1;

fn rebuild(apkg: Vec<u8>, sql: &str) -> Vec<u8> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(apkg)).unwrap();
    let mut db_bytes = Vec::new();
    zip.by_name("collection.anki2")
        .unwrap()
        .read_to_end(&mut db_bytes)
        .unwrap();
    let dir = std::env::temp_dir().join(format!(
        "flash-cloze-test-{}-{}",
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
    out.start_file("media", options).unwrap();
    out.write_all(b"{}").unwrap();
    out.finish().unwrap().into_inner()
}

fn set_fields(front: &str, back: &str) -> String {
    format!(
        "UPDATE notes SET flds = '{}' || char(31) || '{}';",
        front.replace('\'', "''"),
        back.replace('\'', "''")
    )
}

/// Second Anki card for the base note (reversed layout / cloze index 2).
fn add_card(id: i64, ord: i64, queue: i64) -> String {
    format!(
        "INSERT INTO cards (id, nid, did, ord, mod, usn, type, queue, due,
             ivl, factor, reps, lapses, left, odue, odid, flags, data)
         VALUES ({id}, {NOTE_ID}, 2, {ord}, 0, -1, 0, {queue}, 9, 0, 0, 0, 0, 0, 0, 0, 0, '');"
    )
}

fn add_review(id: i64, cid: i64, ease: i64) -> String {
    format!(
        "INSERT INTO revlog (id, cid, usn, ease, ivl, lastIvl, factor, time, type)
         VALUES ({id}, {cid}, -1, {ease}, 1, 1, 2500, 1000, 1);"
    )
}

// ---- cloze ----

#[test]
fn cloze_note_expands_per_index_with_own_history() {
    let day = 24 * 60 * 60 * 1000;
    let sql = format!(
        "{}{}{}{}",
        set_fields(
            "{{c1::Ottawa}} is the capital of {{c2::Canada::country}}",
            "It is on the Ottawa River"
        ),
        add_card(900_001, 1, -1),              // index-2 card, suspended
        add_review(NOW - 5 * day, CARD_ID, 3), // index-1 history
        add_review(NOW - 2 * day, 900_001, 2), // index-2 history
    );
    let parsed = parse_apkg(&rebuild(base_package(), &sql)).unwrap();
    assert_eq!(parsed.rows.len(), 2);

    let c1 = &parsed.rows[0];
    assert_eq!(c1.front, "[...] is the capital of Canada");
    assert_eq!(c1.back, "Ottawa\nIt is on the Ottawa River");
    assert_eq!(c1.cloze_index, Some(1));
    assert!(c1
        .cloze_text
        .as_deref()
        .unwrap()
        .starts_with("{{c1::Ottawa}}"));
    assert_eq!(c1.reviews, vec![(NOW - 5 * day, 3)]);
    assert!(!c1.suspended);
    let html = c1.front_html.as_deref().unwrap();
    assert!(
        html.contains(r#"<span class="cloze-blank">[...]</span>"#),
        "{html}"
    );
    // The reveal shows the completed sentence with the answer highlighted.
    let back_html = c1.back_html.as_deref().unwrap();
    assert!(
        back_html.contains(r#"<span class="cloze-answer">Ottawa</span> is the capital"#),
        "{back_html}"
    );

    let c2 = &parsed.rows[1];
    assert_eq!(c2.front, "Ottawa is the capital of [country]");
    assert_eq!(c2.back, "Canada\nIt is on the Ottawa River");
    assert_eq!(c2.cloze_index, Some(2));
    assert_eq!(c2.reviews, vec![(NOW - 2 * day, 2)]);
    assert!(c2.suspended, "index-2 card was suspended in Anki");

    assert!(parsed
        .messages
        .iter()
        .any(|m| m.contains("1 cloze notes became 2 cards")));
    assert!(parsed
        .messages
        .iter()
        .any(|m| m.contains("1 cards arrive suspended")));
}

// ---- templates ----

/// A three-field notetype with two templates (word -> meaning and a
/// typed reverse), installed over the base note's model id.
fn vocab_model_sql() -> String {
    let model = serde_json::json!({
        "1425279151000": {
            "id": 1425279151000i64, "name": "Vocab", "type": 0,
            "flds": [
                {"name": "Word", "ord": 0}, {"name": "Reading", "ord": 1},
                {"name": "Meaning", "ord": 2},
            ],
            "tmpls": [
                {"ord": 0, "qfmt": "{{Word}} ({{Reading}})",
                 "afmt": "{{FrontSide}}<hr>{{Meaning}}"},
                {"ord": 1, "qfmt": "{{Meaning}} {{type:Word}}",
                 "afmt": "{{type:Word}}<br>{{Reading}}"},
            ],
        }
    });
    format!(
        "UPDATE col SET models = '{}';
         UPDATE notes SET flds = '食べる' || char(31) || 'たべる' || char(31) || 'to eat';",
        model.to_string().replace('\'', "''")
    )
}

#[test]
fn templates_render_multi_field_notes_per_card() {
    let sql = format!("{}{}", vocab_model_sql(), add_card(900_002, 1, 0));
    let parsed = parse_apkg(&rebuild(base_package(), &sql)).unwrap();
    assert_eq!(parsed.rows.len(), 2);

    let forward = &parsed.rows[0];
    assert_eq!(forward.front, "食べる (たべる)");
    assert_eq!(forward.back, "to eat");
    assert!(forward.type_answer.is_none());

    let reverse = &parsed.rows[1];
    assert_eq!(reverse.front, "to eat");
    assert_eq!(reverse.back, "食べる\nたべる");
    assert_eq!(
        reverse.type_answer.as_deref(),
        Some("食べる"),
        "typed field captured"
    );

    assert!(parsed
        .messages
        .iter()
        .any(|m| m.contains("1 additional card layouts")));
}

#[test]
fn notes_without_models_fall_back_with_reversed_heuristic() {
    let sql = format!(
        "UPDATE col SET models = '{{}}';{}{}",
        set_fields("front text", "back text"),
        add_card(900_003, 1, 0),
    );
    let parsed = parse_apkg(&rebuild(base_package(), &sql)).unwrap();
    assert_eq!(parsed.rows.len(), 2);
    assert_eq!(parsed.rows[0].front, "front text");
    assert_eq!(parsed.rows[1].front, "back text");
    assert_eq!(parsed.rows[1].back, "front text");
}

// ---- suspended / buried ----

#[test]
fn suspended_imports_suspended_buried_imports_active() {
    let suspended = parse_apkg(&rebuild(
        base_package(),
        &format!("{}UPDATE cards SET queue = -1;", set_fields("f", "b")),
    ))
    .unwrap();
    assert!(suspended.rows[0].suspended);

    let buried = parse_apkg(&rebuild(
        base_package(),
        &format!("{}UPDATE cards SET queue = -2;", set_fields("f", "b")),
    ))
    .unwrap();
    assert!(
        !buried.rows[0].suspended,
        "burying is temporary, not a user choice"
    );
}

// ---- rich HTML + security ----

#[test]
fn rich_html_survives_sanitized_and_scripts_die() {
    let parsed = parse_apkg(&rebuild(
        base_package(),
        &set_fields(
            r#"<b>bold</b> <span style="color: red" onclick="x()">red</span><script>alert(1)</script>"#,
            r#"<ul><li>one</li><li>two</li></ul><img src=x onerror=alert(2)>"#,
        ),
    ))
    .unwrap();
    let row = &parsed.rows[0];
    assert_eq!(row.front, "bold red");
    assert_eq!(row.back, "• one\n• two");
    let fh = row.front_html.as_deref().unwrap();
    assert!(fh.contains("<b>bold</b>"));
    // Color intent survives as a palette class; the raw style/handler die.
    assert!(fh.contains(r#"<span class="hl-red">red</span>"#), "{fh}");
    assert!(!fh.contains("style") && !fh.contains("onclick") && !fh.contains("alert"));
    let bh = row.back_html.as_deref().unwrap();
    assert!(bh.contains("<li>one</li>"));
    assert!(!bh.contains("onerror") && !bh.contains("<img src"));
}

#[test]
fn hint_buttons_and_font_colors_translate() {
    let parsed = parse_apkg(&rebuild(
        base_package(),
        &set_fields(
            "plain front",
            r#"<font color="hotpink">answer</font>
               <a onclick="document.getElementById('h1').style.display=''">Show mnemonic</a>
               <div id="h1" style="display:none">think of <b>pink</b></div>"#,
        ),
    ))
    .unwrap();
    let bh = parsed.rows[0].back_html.as_deref().unwrap();
    assert!(
        bh.contains(r#"<span class="hl-pink">answer</span>"#),
        "{bh}"
    );
    assert!(
        bh.contains("<details><summary>Show mnemonic</summary>think of <b>pink</b></details>"),
        "{bh}"
    );
    assert!(!bh.contains("onclick") && !bh.contains("display:none"));
    // Plain text keeps the hint audible: "Show mnemonic: think of pink".
    assert!(
        parsed.rows[0].back.contains("Show mnemonic: think of pink"),
        "{}",
        parsed.rows[0].back
    );
}

#[test]
fn notetype_css_class_colors_remap() {
    let model = serde_json::json!({
        "1425279151000": {
            "id": 1425279151000i64, "name": "Styled", "type": 0,
            "css": ".imp { color: red }\n.card { color: black }",
            "flds": [{"name": "Front", "ord": 0}, {"name": "Back", "ord": 1}],
            "tmpls": [{"ord": 0, "qfmt": "{{Front}}", "afmt": "{{Back}}"}],
        }
    });
    let sql = format!(
        "UPDATE col SET models = '{}';
         UPDATE notes SET flds = '<span class=\"imp\">vital</span> term' || char(31) || 'the back';",
        model.to_string().replace('\'', "''")
    );
    let parsed = parse_apkg(&rebuild(base_package(), &sql)).unwrap();
    let fh = parsed.rows[0].front_html.as_deref().unwrap();
    // .imp resolves via the notetype CSS; the foreign class itself dies,
    // the palette class survives. .card (neutral black) maps to nothing.
    assert!(fh.contains(r#"<span class="hl-red">vital</span>"#), "{fh}");
    assert!(!fh.contains("imp"));
}

#[test]
fn plain_cards_store_no_html() {
    let parsed = parse_apkg(&rebuild(
        base_package(),
        &set_fields("just text", "plain back"),
    ))
    .unwrap();
    assert!(parsed.rows[0].front_html.is_none());
    assert!(parsed.rows[0].back_html.is_none());
}

#[test]
fn legacy_latex_delimiters_become_mathjax() {
    let parsed = parse_apkg(&rebuild(
        base_package(),
        &set_fields(
            "area [$]x^2[/$]",
            "sum [$$]\\sum_i i[/$$] and [latex]\\frac12[/latex]",
        ),
    ))
    .unwrap();
    assert_eq!(parsed.rows[0].front, "area \\(x^2\\)");
    assert_eq!(
        parsed.rows[0].back,
        "sum \\[\\sum_i i\\] and \\(\\frac12\\)"
    );
}

// ---- deck options ----

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

#[test]
fn fsrs_deck_options_are_extracted() {
    // DeckConfig.Config: new_per_day(9)=15, desired_retention(37)=0.87,
    // fsrs_params_6(6) = packed floats.
    let mut config = Vec::new();
    config.push(0x48); // field 9, varint
    encode_varint(15, &mut config);
    config.extend_from_slice(&[0xAD, 0x02]); // field 37, wire 5
    config.extend_from_slice(&0.87f32.to_le_bytes());
    let params: Vec<f32> = (0..21).map(|i| 0.1 + i as f32 * 0.05).collect();
    config.push(0x32); // field 6, wire 2
    encode_varint(params.len() as u64 * 4, &mut config);
    for p in &params {
        config.extend_from_slice(&p.to_le_bytes());
    }
    let hex: String = config.iter().map(|b| format!("{b:02X}")).collect();
    let sql = format!(
        "{}CREATE TABLE deck_config (id integer primary key, name text, mtime_secs integer,
             usn integer, config blob);
         INSERT INTO deck_config VALUES (1, 'Default', 0, 0, x'{hex}');",
        set_fields("f", "b")
    );
    let parsed = parse_apkg(&rebuild(base_package(), &sql)).unwrap();
    let settings = parsed.settings.expect("settings found");
    assert_eq!(settings.new_per_day, Some(15));
    assert!((settings.desired_retention.unwrap() - 0.87).abs() < 1e-6);
    assert_eq!(settings.fsrs_params.unwrap().len(), 21);
}

#[test]
fn packages_without_fsrs_data_offer_no_settings() {
    let parsed = parse_apkg(&rebuild(base_package(), &set_fields("f", "b"))).unwrap();
    assert!(parsed.settings.is_none());
}

// ---- round trip: cloze out and back ----

#[test]
fn cloze_cards_export_as_real_cloze_notes_and_reimport() {
    let source = "{{c1::Ottawa}} is in {{c2::Canada}}\u{1f}A geography fact";
    let cards = vec![
        ExportCard {
            deck: "Geo".into(),
            front: "[...] is in Canada".into(),
            back: "Ottawa\nA geography fact".into(),
            cloze: Some((source.to_string(), 1)),
            ..Default::default()
        },
        ExportCard {
            deck: "Geo".into(),
            front: "Ottawa is in [...]".into(),
            back: "Canada\nA geography fact".into(),
            cloze: Some((source.to_string(), 2)),
            ..Default::default()
        },
        ExportCard {
            deck: "Geo".into(),
            front: "plain".into(),
            back: "card".into(),
            ..Default::default()
        },
    ];
    let apkg = build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap();
    let parsed = parse_apkg(&apkg).unwrap();
    assert_eq!(
        parsed.rows.len(),
        3,
        "two cloze cards from one note + one basic"
    );
    let fronts: Vec<&str> = parsed.rows.iter().map(|r| r.front.as_str()).collect();
    assert!(fronts.contains(&"[...] is in Canada"));
    assert!(fronts.contains(&"Ottawa is in [...]"));
    assert!(fronts.contains(&"plain"));
    let c1 = parsed
        .rows
        .iter()
        .find(|r| r.cloze_index == Some(1))
        .unwrap();
    assert_eq!(c1.back, "Ottawa\nA geography fact");
    assert_eq!(c1.deck.as_deref(), Some("Geo"));
}

#[test]
fn rich_html_round_trips_through_export() {
    let cards = vec![ExportCard {
        deck: "Fmt".into(),
        front: "bold word".into(),
        back: "• a\n• b".into(),
        front_html: Some("<b>bold</b> word".into()),
        back_html: Some("<ul><li>a</li><li>b</li></ul>".into()),
        ..Default::default()
    }];
    let parsed =
        parse_apkg(&build_apkg(&cards, &flash_store::export::ExportMedia::default(), NOW).unwrap())
            .unwrap();
    assert_eq!(parsed.rows[0].front, "bold word");
    assert_eq!(parsed.rows[0].back, "• a\n• b");
    assert!(parsed.rows[0]
        .front_html
        .as_deref()
        .unwrap()
        .contains("<b>bold</b>"));
}
