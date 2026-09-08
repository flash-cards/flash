//! Anki .apkg writer: a zip holding a schema-11 `collection.anki2` SQLite
//! file plus the legacy `media` map (`{"0": "cat.png", …}` with the bytes
//! as numbered entries — the format every Anki version reads). Mirrors the reader in `import::apkg` —
//! fields joined with \x1f, tags space-wrapped, everything exported as
//! new. Plain cards become Basic notes (rich HTML preserved when the card
//! carries it); cloze-derived cards re-group by their original source
//! into REAL cloze notes, one Anki card per index — a cloze deck round
//! trips as a cloze deck.

use std::io::{Seek, Write};

use rusqlite::{params, Connection};

use super::{ExportCard, ExportMedia, ExportMediaEntry, ExportMediaPlan};
use crate::tmp::tempdir;

/// Fixed model ids; any values work, Anki keys models by these.
const MODEL_ID: i64 = 1_425_279_151_000;
const CLOZE_MODEL_ID: i64 = 1_425_279_151_001;
/// "Basic (type in the answer)": the same fields as Basic, the answer
/// typed on the front.
const TYPED_MODEL_ID: i64 = 1_425_279_151_002;

/// The package, in memory: for tests and small callers. The server uses
/// `build_apkg_into` with a file sink and one blob in memory at a time.
pub fn build_apkg(
    cards: &[ExportCard],
    media: &ExportMedia,
    now_ms: i64,
) -> Result<Vec<u8>, String> {
    let plan = media.plan();
    let mut read = |entry: &ExportMediaEntry| {
        media
            .files
            .iter()
            .find(|(name, _)| *name == entry.name)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| format!("no bytes for {}", entry.name))
    };
    let cursor = build_apkg_into(
        cards,
        &plan,
        &mut read,
        now_ms,
        std::io::Cursor::new(Vec::new()),
    )?;
    Ok(cursor.into_inner())
}

/// Writes the package to `out`. The collection is built in a temp SQLite
/// file and copied in; each media entry is fetched through `read` right
/// before it is written and dropped right after, so the caller's peak
/// memory is one blob, however many the plan holds. Returns the sink so a
/// file can be reopened for sending.
pub fn build_apkg_into<W: Write + Seek>(
    cards: &[ExportCard],
    plan: &ExportMediaPlan,
    read: &mut dyn FnMut(&ExportMediaEntry) -> Result<Vec<u8>, String>,
    now_ms: i64,
    out: W,
) -> Result<W, String> {
    // SQLite needs a real file; same temp-dir pattern as the importer.
    let dir = tempdir("flash-apkg-export")?;
    let db_path = dir.path.join("collection.anki2");
    let conn = Connection::open(&db_path).map_err(|e| format!("create collection: {e}"))?;
    write_collection(&conn, cards, &plan.names, now_ms)?;
    drop(conn);

    let mut zip = zip::ZipWriter::new(out);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("collection.anki2", options)
        .map_err(|e| format!("zip: {e}"))?;
    let mut db_file = std::fs::File::open(&db_path).map_err(|e| format!("reopen db: {e}"))?;
    std::io::copy(&mut db_file, &mut zip).map_err(|e| format!("zip db: {e}"))?;
    let mut manifest = serde_json::Map::new();
    for (i, entry) in plan.entries.iter().enumerate() {
        let bytes = read(entry)?;
        zip.start_file(i.to_string(), options)
            .map_err(|e| format!("zip: {e}"))?;
        zip.write_all(&bytes)
            .map_err(|e| format!("zip media {}: {e}", entry.name))?;
        drop(bytes);
        manifest.insert(i.to_string(), serde_json::Value::String(entry.name.clone()));
    }
    zip.start_file("media", options)
        .map_err(|e| format!("zip: {e}"))?;
    zip.write_all(serde_json::Value::Object(manifest).to_string().as_bytes())
        .map_err(|e| format!("zip media: {e}"))?;
    zip.finish().map_err(|e| format!("zip finish: {e}"))
}

fn write_collection(
    conn: &Connection,
    cards: &[ExportCard],
    media_names: &std::collections::HashMap<i64, String>,
    now_ms: i64,
) -> Result<(), String> {
    conn.execute_batch(SCHEMA_11)
        .map_err(|e| format!("schema: {e}"))?;

    // Deck name -> Anki deck id (Default keeps 1; ours start at 2).
    let mut deck_ids: Vec<(String, i64)> = Vec::new();
    for card in cards {
        if !deck_ids.iter().any(|(name, _)| name == &card.deck) {
            deck_ids.push((card.deck.clone(), 2 + deck_ids.len() as i64));
        }
    }

    let now_s = now_ms / 1000;
    let mut decks_json = serde_json::json!({
        "1": deck_json("Default", 1, now_s),
    });
    for (name, id) in &deck_ids {
        decks_json[id.to_string()] = deck_json(name, *id, now_s);
    }
    let models_json = serde_json::json!({
        MODEL_ID.to_string(): basic_model(now_s),
        CLOZE_MODEL_ID.to_string(): cloze_model(now_s),
        TYPED_MODEL_ID.to_string(): typed_model(now_s),
    });
    let conf_json = serde_json::json!({
        "curDeck": 1, "activeDecks": [1], "newSpread": 0, "collapseTime": 1200,
        "timeLim": 0, "estTimes": true, "dueCounts": true,
        "curModel": MODEL_ID.to_string(), "nextPos": 1,
        "sortType": "noteFld", "sortBackwards": false, "addToCur": true,
        "dayLearnFirst": false,
    });
    let dconf_json = serde_json::json!({ "1": default_deck_conf(now_s) });

    conn.execute(
        "INSERT INTO col (id, crt, mod, scm, ver, dty, usn, ls, conf, models, decks, dconf, tags)
         VALUES (1, ?1, ?2, ?2, 11, 0, 0, 0, ?3, ?4, ?5, ?6, '{}')",
        params![
            now_s,
            now_ms,
            conf_json.to_string(),
            models_json.to_string(),
            decks_json.to_string(),
            dconf_json.to_string(),
        ],
    )
    .map_err(|e| format!("col row: {e}"))?;

    let mut note_stmt = conn
        .prepare(
            "INSERT INTO notes (id, guid, mid, mod, usn, tags, flds, sfld, csum, flags, data)
             VALUES (?1, ?2, ?3, ?4, -1, ?5, ?6, ?7, ?8, 0, '')",
        )
        .map_err(|e| format!("notes stmt: {e}"))?;
    let mut card_stmt = conn
        .prepare(
            "INSERT INTO cards (id, nid, did, ord, mod, usn, type, queue, due,
                                ivl, factor, reps, lapses, left, odue, odid, flags, data)
             VALUES (?1, ?2, ?3, ?4, ?5, -1, 0, 0, ?6, 0, 0, 0, 0, 0, 0, 0, 0, '')",
        )
        .map_err(|e| format!("cards stmt: {e}"))?;

    // Group cloze-derived cards back into one note per original source;
    // everything else is a Basic note with its own card.
    enum Unit<'a> {
        Basic(&'a ExportCard),
        Cloze {
            source: &'a str,
            members: Vec<(&'a ExportCard, u32)>,
        },
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut cloze_at: std::collections::HashMap<(&str, &str), usize> =
        std::collections::HashMap::new();
    for card in cards {
        match &card.cloze {
            Some((source, index)) => {
                let key = (card.deck.as_str(), source.as_str());
                let at = *cloze_at.entry(key).or_insert_with(|| {
                    units.push(Unit::Cloze {
                        source: source.as_str(),
                        members: Vec::new(),
                    });
                    units.len() - 1
                });
                if let Unit::Cloze { members, .. } = &mut units[at] {
                    if !members.iter().any(|(_, i)| i == index) {
                        members.push((card, *index));
                    }
                }
            }
            None => units.push(Unit::Basic(card)),
        }
    }

    let did_of = |deck: &str| {
        deck_ids
            .iter()
            .find(|(name, _)| name == deck)
            .map(|(_, id)| *id)
            .unwrap_or(1)
    };
    let tags_of = |card: &ExportCard| {
        if card.tags.is_empty() {
            String::new()
        } else {
            format!(" {} ", card.tags.join(" "))
        }
    };

    // A running counter keeps note and card ids unique even when one
    // cloze note fans out into several cards.
    let mut next_id = now_ms;
    let mut due = 0i64;
    for unit in &units {
        match unit {
            Unit::Basic(card) => {
                let note_id = next_id;
                next_id += 2;
                due += 1;
                let model_id = if card.type_answer.is_some() {
                    TYPED_MODEL_ID
                } else {
                    MODEL_ID
                };
                let front_plain = escape_field(&card.front);
                // Rich HTML (already sanitized at import) round-trips the
                // structure; plain cards get escaped text.
                let front_fld = card
                    .front_html
                    .as_deref()
                    .map(|h| rewrite_media_refs(h, media_names))
                    .unwrap_or_else(|| front_plain.clone());
                let back_fld = card
                    .back_html
                    .as_deref()
                    .map(|h| rewrite_media_refs(h, media_names))
                    .unwrap_or_else(|| escape_field(&card.back));
                note_stmt
                    .execute(params![
                        note_id,
                        format!("flash{note_id}"),
                        model_id,
                        now_s,
                        tags_of(card),
                        format!("{front_fld}\u{1f}{back_fld}"),
                        front_plain,
                        field_checksum(&front_plain),
                    ])
                    .map_err(|e| format!("note insert: {e}"))?;
                card_stmt
                    .execute(params![
                        note_id + 1,
                        note_id,
                        did_of(&card.deck),
                        0,
                        now_s,
                        due
                    ])
                    .map_err(|e| format!("card insert: {e}"))?;
            }
            Unit::Cloze { source, members } => {
                let note_id = next_id;
                next_id += 1 + members.len() as i64;
                let (text, extra) = source.split_once('\u{1f}').unwrap_or((source, ""));
                let text_fld = escape_field(text);
                let flds = format!("{text_fld}\u{1f}{}", escape_field(extra));
                let tags = members.first().map(|(c, _)| tags_of(c)).unwrap_or_default();
                note_stmt
                    .execute(params![
                        note_id,
                        format!("flash{note_id}"),
                        CLOZE_MODEL_ID,
                        now_s,
                        tags,
                        flds,
                        text_fld,
                        field_checksum(&text_fld),
                    ])
                    .map_err(|e| format!("note insert: {e}"))?;
                for (offset, (card, index)) in members.iter().enumerate() {
                    due += 1;
                    card_stmt
                        .execute(params![
                            note_id + 1 + offset as i64,
                            note_id,
                            did_of(&card.deck),
                            (*index as i64 - 1).max(0),
                            now_s,
                            due
                        ])
                        .map_err(|e| format!("card insert: {e}"))?;
                }
            }
        }
    }
    Ok(())
}

/// Turns Flash's served-media markup back into Anki's: `<img
/// src="/media/7" …>` becomes `<img src="cat.png">`, and `<audio|video …
/// src="/media/7"></audio|video>` becomes `[sound:cat.mp3]`. Refs whose id
/// isn't in `names` (blob unavailable) keep the id as a plain filename so
/// nothing silently disappears from the card.
pub fn rewrite_media_refs(html: &str, names: &std::collections::HashMap<i64, String>) -> String {
    const MARK: &str = "src=\"/media/";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        // Find the enclosing tag.
        let Some(lt) = rest[..at].rfind('<') else {
            out.push_str(&rest[..at + MARK.len()]);
            rest = &rest[at + MARK.len()..];
            continue;
        };
        let Some(gt_rel) = rest[at..].find('>') else {
            break;
        };
        let tag_end = at + gt_rel + 1;
        let tag = &rest[lt..tag_end];
        let tag_name: String = tag[1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let id_str: String = rest[at + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let name = id_str
            .parse::<i64>()
            .ok()
            .and_then(|id| names.get(&id).cloned())
            .unwrap_or_else(|| format!("media-{id_str}"));
        out.push_str(&rest[..lt]);
        let mut consumed = tag_end;
        match tag_name.as_str() {
            "audio" | "video" => {
                let close = format!("</{tag_name}>");
                if rest[tag_end..].trim_start().starts_with(&close) {
                    let ws = rest[tag_end..].len() - rest[tag_end..].trim_start().len();
                    consumed = tag_end + ws + close.len();
                }
                out.push_str(&format!("[sound:{name}]"));
            }
            _ => {
                // Keep alt text when present; drop everything else.
                let alt = attr(tag, "alt");
                match alt {
                    Some(alt) => {
                        out.push_str(&format!("<img src=\"{}\" alt=\"{alt}\">", esc(&name)))
                    }
                    None => out.push_str(&format!("<img src=\"{}\">", esc(&name))),
                }
            }
        }
        rest = &rest[consumed..];
    }
    out.push_str(rest);
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')?;
    Some(&tag[start..start + end])
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// Mirror of the importer's strip_html: HTML-escape text so markup-ish
/// card content survives, newlines become <br>.
fn escape_field(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>")
}

/// Anki's duplicate-detection checksum: first 8 hex chars of SHA-1 of the
/// sort field, as an integer.
fn field_checksum(sfld: &str) -> i64 {
    let hex = sha1_smol::Sha1::from(sfld.as_bytes()).digest().to_string();
    i64::from_str_radix(&hex[..8], 16).unwrap_or(0)
}

fn deck_json(name: &str, id: i64, now_s: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id, "name": name, "desc": "", "mod": now_s, "usn": -1,
        "collapsed": false, "browserCollapsed": false, "dyn": 0, "conf": 1,
        "extendNew": 10, "extendRev": 50,
        "newToday": [0, 0], "revToday": [0, 0], "lrnToday": [0, 0], "timeToday": [0, 0],
    })
}

/// Styles Flash's remapped highlight classes so exported cards keep their
/// colors in Anki (readable on Anki's default white card).
const HL_CSS: &str = "\n.hl-red { color: #c04347; }\n.hl-amber { color: #9a6c10; }\n.hl-green { color: #1d7a52; }\n.hl-teal { color: #12756d; }\n.hl-blue { color: #2f6cb3; }\n.hl-purple { color: #6d4fb3; }\n.hl-pink { color: #ad4a80; }\n.hl-bg-red { background-color: #f6dcdd; }\n.hl-bg-amber { background-color: #f3e6c8; }\n.hl-bg-green { background-color: #d8ecdf; }\n.hl-bg-teal { background-color: #d5eae7; }\n.hl-bg-blue { background-color: #dbe6f3; }\n.hl-bg-purple { background-color: #e5ddf3; }\n.hl-bg-pink { background-color: #f2dde9; }";

fn basic_model(now_s: i64) -> serde_json::Value {
    let css = format!(
        ".card {{ font-family: arial; font-size: 20px; text-align: center; color: black; background-color: white; }}{HL_CSS}"
    );
    serde_json::json!({
        "id": MODEL_ID, "name": "Basic", "type": 0, "mod": now_s, "usn": -1,
        "sortf": 0, "did": 1, "css": css,
        "latexPre": "\\documentclass[12pt]{article}\n\\special{papersize=3in,5in}\n\\usepackage[utf8]{inputenc}\n\\usepackage{amssymb,amsmath}\n\\pagestyle{empty}\n\\setlength{\\parindent}{0in}\n\\begin{document}\n",
        "latexPost": "\\end{document}",
        "flds": [
            { "name": "Front", "ord": 0, "sticky": false, "rtl": false, "font": "Arial", "size": 20, "media": [] },
            { "name": "Back", "ord": 1, "sticky": false, "rtl": false, "font": "Arial", "size": 20, "media": [] },
        ],
        "tmpls": [{
            "name": "Card 1", "ord": 0, "did": null,
            "qfmt": "{{Front}}", "afmt": "{{FrontSide}}<hr id=answer>{{Back}}",
            "bqfmt": "", "bafmt": "",
        }],
        "req": [[0, "any", [0]]],
        "tags": [], "vers": [],
    })
}

fn typed_model(now_s: i64) -> serde_json::Value {
    let mut model = basic_model(now_s);
    model["id"] = serde_json::json!(TYPED_MODEL_ID);
    model["name"] = serde_json::json!("Basic (type in the answer)");
    model["tmpls"] = serde_json::json!([{
        "name": "Card 1", "ord": 0, "did": null,
        "qfmt": "{{Front}}\n\n{{type:Back}}",
        "afmt": "{{FrontSide}}\n\n<hr id=answer>\n\n{{type:Back}}",
        "bqfmt": "", "bafmt": "",
    }]);
    model
}

fn cloze_model(now_s: i64) -> serde_json::Value {
    let css = format!(
        ".card {{ font-family: arial; font-size: 20px; text-align: center; color: black; background-color: white; }}\n.cloze {{ font-weight: bold; color: blue; }}{HL_CSS}"
    );
    serde_json::json!({
        "id": CLOZE_MODEL_ID, "name": "Cloze", "type": 1, "mod": now_s, "usn": -1,
        "sortf": 0, "did": 1,
        "css": css,
        "latexPre": "\\documentclass[12pt]{article}\n\\special{papersize=3in,5in}\n\\usepackage[utf8]{inputenc}\n\\usepackage{amssymb,amsmath}\n\\pagestyle{empty}\n\\setlength{\\parindent}{0in}\n\\begin{document}\n",
        "latexPost": "\\end{document}",
        "flds": [
            { "name": "Text", "ord": 0, "sticky": false, "rtl": false, "font": "Arial", "size": 20, "media": [] },
            { "name": "Extra", "ord": 1, "sticky": false, "rtl": false, "font": "Arial", "size": 20, "media": [] },
        ],
        "tmpls": [{
            "name": "Cloze", "ord": 0, "did": null,
            "qfmt": "{{cloze:Text}}", "afmt": "{{cloze:Text}}<br>{{Extra}}",
            "bqfmt": "", "bafmt": "",
        }],
        "req": [],
        "tags": [], "vers": [],
    })
}

fn default_deck_conf(now_s: i64) -> serde_json::Value {
    serde_json::json!({
        "id": 1, "name": "Default", "mod": now_s, "usn": -1, "autoplay": true,
        "dyn": false, "maxTaken": 60, "replayq": true, "timer": 0,
        "new": { "bury": false, "delays": [1.0, 10.0], "initialFactor": 2500,
                 "ints": [1, 4, 0], "order": 1, "perDay": 20 },
        "rev": { "bury": false, "ease4": 1.3, "hardFactor": 1.2, "ivlFct": 1.0,
                 "maxIvl": 36500, "perDay": 200 },
        "lapse": { "delays": [10.0], "leechAction": 1, "leechFails": 8,
                   "minInt": 1, "mult": 0.0 },
    })
}

const SCHEMA_11: &str = r#"
CREATE TABLE col (
  id integer primary key, crt integer not null, mod integer not null,
  scm integer not null, ver integer not null, dty integer not null,
  usn integer not null, ls integer not null, conf text not null,
  models text not null, decks text not null, dconf text not null,
  tags text not null
);
CREATE TABLE notes (
  id integer primary key, guid text not null, mid integer not null,
  mod integer not null, usn integer not null, tags text not null,
  flds text not null, sfld text not null, csum integer not null,
  flags integer not null, data text not null
);
CREATE TABLE cards (
  id integer primary key, nid integer not null, did integer not null,
  ord integer not null, mod integer not null, usn integer not null,
  type integer not null, queue integer not null, due integer not null,
  ivl integer not null, factor integer not null, reps integer not null,
  lapses integer not null, left integer not null, odue integer not null,
  odid integer not null, flags integer not null, data text not null
);
CREATE TABLE revlog (
  id integer primary key, cid integer not null, usn integer not null,
  ease integer not null, ivl integer not null, lastIvl integer not null,
  factor integer not null, time integer not null, type integer not null
);
CREATE TABLE graves (
  usn integer not null, oid integer not null, type integer not null
);
CREATE INDEX ix_notes_usn ON notes (usn);
CREATE INDEX ix_cards_usn ON cards (usn);
CREATE INDEX ix_revlog_usn ON revlog (usn);
CREATE INDEX ix_cards_nid ON cards (nid);
CREATE INDEX ix_cards_sched ON cards (did, queue, due);
CREATE INDEX ix_revlog_cid ON revlog (cid);
CREATE INDEX ix_notes_csum ON notes (csum);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> std::collections::HashMap<i64, String> {
        [
            (7, "cat.png".to_string()),
            (8, "moo.mp3".to_string()),
            (9, "clip.mp4".to_string()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn images_and_sounds_round_trip_to_anki_markup() {
        let html = r#"<p>Look <img src="/media/7" alt="cat.png"> and hear <audio controls preload="none" src="/media/8"></audio>.</p><video controls preload="none" src="/media/9"></video>"#;
        assert_eq!(
            rewrite_media_refs(html, &names()),
            r#"<p>Look <img src="cat.png" alt="cat.png"> and hear [sound:moo.mp3].</p>[sound:clip.mp4]"#
        );
    }

    #[test]
    fn unknown_ids_keep_a_visible_filename_and_plain_html_is_untouched() {
        assert_eq!(
            rewrite_media_refs(r#"<img src="/media/42">"#, &names()),
            r#"<img src="media-42">"#
        );
        let plain = "<b>no media</b> here";
        assert_eq!(rewrite_media_refs(plain, &names()), plain);
    }
}
