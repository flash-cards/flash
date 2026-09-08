//! Anki deck-options import — the FSRS-meaningful subset only. SM-2 knobs
//! (learning steps, ease, interval modifiers) are exactly what FSRS
//! replaces, so what transfers is: tuned FSRS parameters, desired
//! retention, and the new-cards/day limit. Offered as an opt-in at import
//! only when the deck actually carries FSRS data (a bare default preset is
//! noise, not signal).

use rusqlite::Connection;

use super::proto::{packed_floats, read_bytes, read_f32, read_varint, skip_field};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ImportedSettings {
    pub desired_retention: Option<f32>,
    pub fsrs_params: Option<Vec<f32>>,
    pub new_per_day: Option<u32>,
}

impl ImportedSettings {
    /// Worth offering only when real FSRS data came along.
    pub fn is_meaningful(&self) -> bool {
        self.desired_retention.is_some() || self.fsrs_params.is_some()
    }
}

/// Best-effort read of the package's deck presets. With several presets,
/// the one carrying FSRS parameters wins (ties: first seen).
pub fn load_deck_options(conn: &Connection) -> Option<ImportedSettings> {
    let found = load_modern(conn);
    let best = found.into_iter().reduce(|a, b| {
        if b.fsrs_params.is_some() && a.fsrs_params.is_none() {
            b
        } else {
            a
        }
    })?;
    best.is_meaningful().then_some(best)
}

/// deck_config table, DeckConfig.Config protobuf:
/// new_per_day = 9 (varint), desired_retention = 37 (float),
/// fsrs_params_6 = 6 / fsrs_params_5 = 5 / fsrs_params_4 = 3 (packed floats).
fn load_modern(conn: &Connection) -> Vec<ImportedSettings> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT config FROM deck_config ORDER BY id LIMIT {}",
        super::MAX_LOOKUP_ROWS
    )) else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)) else {
        return out;
    };
    for config in rows.flatten() {
        out.push(config_settings(&config));
    }
    out
}

/// One `DeckConfig.Config` protobuf blob as the settings it carries.
pub(crate) fn config_settings(config: &[u8]) -> ImportedSettings {
    let mut s = ImportedSettings::default();
    let mut params_4: Option<Vec<f32>> = None;
    let mut params_5: Option<Vec<f32>> = None;
    let mut params_6: Option<Vec<f32>> = None;
    let mut i = 0;
    while i < config.len() {
        let Some(tag) = read_varint(config, &mut i) else {
            break;
        };
        match (tag >> 3, tag & 7) {
            (9, 0) => {
                s.new_per_day = read_varint(config, &mut i).map(|v| v.min(u32::MAX as u64) as u32)
            }
            (37, 5) => s.desired_retention = read_f32(config, &mut i),
            (3, 2) => params_4 = read_bytes(config, &mut i).map(packed_floats),
            (5, 2) => params_5 = read_bytes(config, &mut i).map(packed_floats),
            (6, 2) => params_6 = read_bytes(config, &mut i).map(packed_floats),
            (_, wire) => {
                if !skip_field(config, &mut i, wire) {
                    break;
                }
            }
        }
    }
    // Newest generation wins; empty vectors are absent, not data.
    s.fsrs_params = [params_6, params_5, params_4]
        .into_iter()
        .flatten()
        .find(|p| !p.is_empty());
    // Sanity: retention must be a probability.
    if let Some(r) = s.desired_retention {
        if !(0.5..=0.995).contains(&r) {
            s.desired_retention = None;
        }
    }
    s
}
