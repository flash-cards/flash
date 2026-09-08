//! Notetype definitions from both collection generations: field names,
//! card templates (qfmt/afmt), and CSS color rules. Cloze-ness is decided
//! from the templates, not the notetype kind flag. Modern schema stores
//! these across `notetypes`/`fields`/`templates` tables with protobuf
//! configs; legacy keeps a JSON blob in `col.models`. Best-effort:
//! anything unreadable simply yields no notetype and the importer falls
//! back to first-two-fields behavior.

use std::collections::HashMap;

use rusqlite::Connection;

use super::proto::{read_bytes, read_varint, skip_field};

#[derive(Debug, Clone, Default)]
pub struct TemplatePair {
    pub qfmt: String,
    pub afmt: String,
}

#[derive(Debug, Clone, Default)]
pub struct Notetype {
    /// Field names, ord order.
    pub fields: Vec<String>,
    /// Card templates, ord order.
    pub templates: Vec<TemplatePair>,
    /// Simple class->color rules mined from the notetype CSS blob, already
    /// mapped to Flash `hl-*` classes (best effort; empty when none).
    pub class_colors: super::colorize::ClassColorMap,
}

pub fn load_notetypes(conn: &Connection) -> HashMap<i64, Notetype> {
    let modern = load_modern(conn);
    if !modern.is_empty() {
        return modern;
    }
    load_legacy(conn).unwrap_or_default()
}

fn load_modern(conn: &Connection) -> HashMap<i64, Notetype> {
    let mut map: HashMap<i64, Notetype> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT id, config FROM notetypes LIMIT {}",
        super::MAX_LOOKUP_ROWS
    )) else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
    else {
        return map;
    };
    for (id, config) in rows.flatten() {
        map.insert(
            id,
            Notetype {
                class_colors: super::colorize::parse_css_class_colors(&notetype_css(&config)),
                ..Default::default()
            },
        );
    }
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT ntid, name FROM fields ORDER BY ntid, ord LIMIT {}",
        super::MAX_LOOKUP_ROWS
    )) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        {
            for (ntid, name) in rows.flatten() {
                if let Some(nt) = map.get_mut(&ntid) {
                    nt.fields.push(name);
                }
            }
        }
    }
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT ntid, config FROM templates ORDER BY ntid, ord LIMIT {}",
        super::MAX_LOOKUP_ROWS
    )) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        {
            for (ntid, config) in rows.flatten() {
                if let Some(nt) = map.get_mut(&ntid) {
                    nt.templates.push(template_formats(&config));
                }
            }
        }
    }
    map
}

/// Notetype.Config.css — length-delimited field 3.
pub(crate) fn notetype_css(config: &[u8]) -> String {
    let mut i = 0;
    while i < config.len() {
        let Some(tag) = read_varint(config, &mut i) else {
            return String::new();
        };
        match (tag >> 3, tag & 7) {
            (3, 2) => {
                return read_bytes(config, &mut i)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default()
            }
            (_, wire) => {
                if !skip_field(config, &mut i, wire) {
                    return String::new();
                }
            }
        }
    }
    String::new()
}

/// Notetype.Template.Config — q_format field 1, a_format field 2.
pub(crate) fn template_formats(config: &[u8]) -> TemplatePair {
    let mut out = TemplatePair::default();
    let mut i = 0;
    while i < config.len() {
        let Some(tag) = read_varint(config, &mut i) else {
            break;
        };
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                let Some(b) = read_bytes(config, &mut i) else {
                    break;
                };
                out.qfmt = String::from_utf8_lossy(b).into_owned();
            }
            (2, 2) => {
                let Some(b) = read_bytes(config, &mut i) else {
                    break;
                };
                out.afmt = String::from_utf8_lossy(b).into_owned();
            }
            (_, wire) => {
                if !skip_field(config, &mut i, wire) {
                    break;
                }
            }
        }
    }
    out
}

fn load_legacy(conn: &Connection) -> Option<HashMap<i64, Notetype>> {
    let json: String = conn
        .query_row("SELECT models FROM col LIMIT 1", [], |r| r.get(0))
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let obj = value.as_object()?;
    let mut map = HashMap::new();
    for (mid, model) in obj {
        let Ok(mid) = mid.parse::<i64>() else {
            continue;
        };
        let mut fields: Vec<(i64, String)> = model["flds"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        Some((
                            f["ord"].as_i64().unwrap_or(0),
                            f["name"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        fields.sort_by_key(|(ord, _)| *ord);
        let mut templates: Vec<(i64, TemplatePair)> = model["tmpls"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| {
                        (
                            t["ord"].as_i64().unwrap_or(0),
                            TemplatePair {
                                qfmt: t["qfmt"].as_str().unwrap_or("").to_string(),
                                afmt: t["afmt"].as_str().unwrap_or("").to_string(),
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        templates.sort_by_key(|(ord, _)| *ord);
        map.insert(
            mid,
            Notetype {
                fields: fields.into_iter().map(|(_, n)| n).collect(),
                templates: templates.into_iter().map(|(_, t)| t).collect(),
                class_colors: super::colorize::parse_css_class_colors(
                    model["css"].as_str().unwrap_or(""),
                ),
            },
        );
    }
    Some(map)
}
