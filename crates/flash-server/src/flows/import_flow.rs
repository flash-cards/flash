//! Import (CSV or Anki .apkg) in two steps: `preview` parses the upload
//! and parks it as a token-named pending file in the data dir; `commit`
//! reads it back, ingests media, applies the user's choices, and inserts
//! through `Services::import_cards` (one cap decision for the batch).
//! Both surfaces show the same preview and accept the same choices.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use flash_core::UserId;
use flash_store::import::{parse_apkg, parse_csv, ImportRow, ImportedSettings};

use super::Failure;
use crate::auth::{hash_token, new_token};
use crate::media_store::MediaStore;
use crate::service::{now_ms, Services};
use crate::state::HeavyPermit;

/// The largest upload either surface accepts (the body limit).
pub const MAX_BYTES: usize = 100 * 1024 * 1024;

/// What the preview step keeps on disk between the two requests.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingImport {
    user_id: i64,
    default_deck: String,
    rows: Vec<ImportRow>,
    messages: Vec<String>,
    skipped: u32,
    /// FSRS-relevant deck options found in the package.
    #[serde(default)]
    settings: Option<ImportedSettings>,
    /// The uploaded .apkg was kept next to the pending JSON so commit can
    /// ingest its media.
    #[serde(default)]
    media_apkg: bool,
}

/// What the preview shows and what commit needs back (`token`).
#[derive(Debug, Clone)]
pub struct ImportPreview {
    pub total: usize,
    /// What was uploaded, echoed back since a file input can't be re-filled.
    pub file_name: String,
    pub file_size_bytes: usize,
    /// Prefill for the required destination field (may be empty).
    pub deck: String,
    /// Extra line when the file names several decks that will merge.
    pub deck_note: Option<String>,
    pub warnings: u32,
    pub sample: Vec<(String, String)>,
    pub messages: Vec<String>,
    pub token: String,
    /// Total Anki reviews found — >0 offers the bring-your-progress box.
    pub progress_reviews: usize,
    /// Human summary of importable Anki FSRS settings, when found.
    pub settings_desc: Option<String>,
    /// Cards whose deck colors were remapped to the Flash palette — >0
    /// offers the keep-colors/clean-look choice.
    pub colored_cards: usize,
}

/// The user's choices on the preview.
#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub token: String,
    /// Destination deck, as edited on the preview. Required.
    pub deck: String,
    /// Bring the Anki study progress along.
    pub progress: bool,
    /// Adopt the package's FSRS settings.
    pub settings: bool,
    /// Strip the remapped deck colors ("clean look").
    pub clean_colors: bool,
}

pub fn dir(config: &crate::config::Config) -> PathBuf {
    config.data_dir.join("imports")
}

/// How long an uncommitted preview (and its kept .apkg) is retained. A
/// preview is committed within minutes or abandoned; two hours covers a
/// long pause without keeping 100 MB packages around all day.
const PENDING_TTL_MS: i64 = 2 * 60 * 60 * 1000;

/// The two files a pending import may leave: the parsed rows, and the
/// original package when media must be ingested at commit. Both carry
/// the owner in the name so a user's previous preview can be found and
/// replaced without opening it.
fn pending_paths(dir: &Path, user: UserId, token: &str) -> (PathBuf, PathBuf) {
    let stem = format!("{}-{}", user.raw(), hash_token(token));
    (
        dir.join(format!("{stem}.json")),
        dir.join(format!("{stem}.apkg")),
    )
}

/// Abandoned previews age out after `PENDING_TTL_MS`.
fn cleanup(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_millis() as i64 > PENDING_TTL_MS);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One pending import per user: a new preview replaces whatever the same
/// user left behind, so repeated previews cannot accumulate packages on
/// disk (the volume also holds the database).
fn discard_pending_for(dir: &Path, user: UserId) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{}-", user.raw());
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&prefix))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn settings_desc(s: &ImportedSettings) -> String {
    let mut parts = Vec::new();
    if let Some(r) = s.desired_retention {
        parts.push(format!("{:.0}% retention", r * 100.0));
    }
    if let Some(p) = &s.fsrs_params {
        parts.push(format!("tuned FSRS parameters ({} values)", p.len()));
    }
    if let Some(n) = s.new_per_day {
        parts.push(format!("{n} new cards/day"));
    }
    parts.join(", ")
}

/// "Pharm" for ["Pharm::Cardio", "Pharm::Renal"]; the first name when
/// they share nothing.
fn common_deck_parent(decks: &[String]) -> String {
    let first: Vec<&str> = decks[0].split("::").collect();
    let mut shared = first.len();
    for d in &decks[1..] {
        let parts: Vec<&str> = d.split("::").collect();
        shared = shared.min(parts.iter().zip(&first).take_while(|(a, b)| a == b).count());
    }
    if shared == 0 {
        decks[0].clone()
    } else {
        first[..shared].join("::")
    }
}

fn merge_note(file_decks: &[String]) -> Option<String> {
    (file_decks.len() > 1).then(|| {
        format!(
            "The file holds {} decks ({}); all of their cards go into the deck named above.",
            file_decks.len(),
            file_decks.join(", ")
        )
    })
}

/// The most cards one file may bring: enough for the largest shared Anki
/// decks, small enough that one commit cannot hold the connection for
/// minutes.
pub const MAX_ROWS: usize = flash_store::import::MAX_ROWS;

/// The importer's verdict on an upload, split into the two voices: the
/// public sentence goes back as `Failure::User`; whatever the parser
/// saw underneath is logged here and nowhere else.
fn parse_failed(user: UserId, filename: &str, err: flash_store::import::ImportError) -> Failure {
    if !err.detail().is_empty() {
        tracing::warn!(
            user = user.raw(),
            file = ?filename,
            detail = err.detail(),
            "import parse failed: {}",
            err.public()
        );
    }
    Failure::user(err.public())
}

/// Parses the upload and parks it. `Failure::User` messages are for the
/// uploader, `Failure::Internal` for the log. The permit names the user:
/// this is a heavy job, and only its holder runs one.
pub fn preview(
    permit: &HeavyPermit,
    dir: &Path,
    filename: &str,
    bytes: &[u8],
    deck_override: &str,
) -> Result<ImportPreview, Failure> {
    let user = permit.user();
    let started = std::time::Instant::now();
    let is_apkg = filename.to_lowercase().ends_with(".apkg");
    let parsed = if is_apkg {
        parse_apkg(bytes).map_err(|e| parse_failed(user, filename, e))?
    } else {
        parse_csv(bytes).map_err(|e| parse_failed(user, filename, e))?
    };
    tracing::info!(
        user = user.raw(),
        file = ?filename,
        bytes = bytes.len(),
        rows = parsed.rows.len(),
        media_referenced = parsed.media.referenced(),
        parse_ms = started.elapsed().as_millis() as u64,
        "import preview parsed"
    );
    if parsed.rows.len() > MAX_ROWS {
        return Err(Failure::user(format!(
            "that file has {} cards; imports are capped at {MAX_ROWS} per file — split it in Anki first",
            parsed.rows.len()
        )));
    }
    if parsed.rows.is_empty() {
        return Err(Failure::user(format!(
            "no importable cards found ({} rows skipped)",
            parsed.skipped
        )));
    }
    let token = new_token();
    std::fs::create_dir_all(dir)?;
    cleanup(dir);
    discard_pending_for(dir, user);
    let (path, apkg_path) = pending_paths(dir, user, &token);
    // Media ingest at commit needs the original package.
    let keep_media_apkg = is_apkg && parsed.media.total_bytes > 0 && parsed.media.referenced() > 0;
    if keep_media_apkg {
        std::fs::write(&apkg_path, bytes)?;
    }
    let file_decks: Vec<String> = {
        let mut names: Vec<String> = Vec::new();
        for r in &parsed.rows {
            if let Some(d) = &r.deck {
                if !names.contains(d) {
                    names.push(d.clone());
                }
            }
        }
        names
    };
    // Prefill for the editable, required field on the preview: what the
    // user typed, else the file's deck, else the shared parent of the
    // file's decks ("Pharm" for Pharm::Cardio + Pharm::Renal), else blank.
    let (destination, deck_note) = if !deck_override.is_empty() {
        (deck_override.to_string(), merge_note(&file_decks))
    } else {
        match file_decks.len() {
            0 => (String::new(), None),
            1 => (file_decks[0].clone(), None),
            _ => (common_deck_parent(&file_decks), merge_note(&file_decks)),
        }
    };
    let pending = PendingImport {
        user_id: user.raw(),
        default_deck: destination.clone(),
        rows: parsed.rows,
        messages: parsed.messages.clone(),
        skipped: parsed.skipped,
        settings: parsed.settings.clone(),
        media_apkg: keep_media_apkg,
    };
    std::fs::write(&path, serde_json::to_vec(&pending)?)?;

    let progress_reviews = pending.rows.iter().map(|r| r.reviews.len()).sum();
    let has_hl = |h: &Option<flash_store::richtext::SanitizedHtml>| {
        h.as_ref().is_some_and(|h| h.contains("hl-"))
    };
    let colored_cards = pending
        .rows
        .iter()
        .filter(|r| has_hl(&r.front_html) || has_hl(&r.back_html))
        .count();
    Ok(ImportPreview {
        total: pending.rows.len(),
        file_name: filename.to_string(),
        file_size_bytes: bytes.len(),
        deck: destination,
        deck_note,
        warnings: pending.skipped,
        sample: pending
            .rows
            .iter()
            .take(5)
            .map(|r| (r.front.clone(), r.back.clone()))
            .collect(),
        messages: pending.messages,
        token,
        progress_reviews,
        settings_desc: pending
            .settings
            .as_ref()
            .map(settings_desc)
            .filter(|d| !d.is_empty()),
        colored_cards,
    })
}

/// Reads the pending import back, ingests its media, applies the
/// choices, and inserts. Returns how many cards were added. `Err`
/// messages are user-facing.
pub fn commit(
    permit: &HeavyPermit,
    services: &Services,
    media_store: &dyn MediaStore,
    dir: &Path,
    is_admin: bool,
    opts: &CommitOptions,
) -> Result<usize, Failure> {
    let user = permit.user();
    // The path carries the caller's id, so another user's token cannot
    // even name this user's file; the owner check below is belt and braces.
    let (path, apkg_path) = pending_paths(dir, user, &opts.token);
    let bytes = std::fs::read(&path).map_err(|_| Failure::user("preview expired; upload again"))?;
    let mut pending: PendingImport = serde_json::from_str(&String::from_utf8_lossy(&bytes))?;
    if pending.user_id != user.raw() {
        return Err(Failure::user("preview belongs to another user"));
    }
    let deck = opts.deck.trim().to_string();
    if deck.is_empty() {
        return Err(Failure::user("deck name required"));
    }
    pending.default_deck = deck;
    for row in &mut pending.rows {
        row.deck = None;
    }

    // Media ingestion: pull the referenced files out of the kept
    // package, then upgrade the rows' placeholder chips into real
    // /media/ elements.
    let commit_started = std::time::Instant::now();
    let media_map = if pending.media_apkg {
        std::fs::read(&apkg_path).ok().map(|package| {
            crate::media::ingest_package_media(
                permit,
                services,
                media_store,
                is_admin,
                &package,
                &pending.rows,
                now_ms(),
            )
        })
    } else {
        None
    };
    if let Some(map) = &media_map {
        if !map.is_empty() {
            let resolve: HashMap<String, i64> =
                map.iter().map(|(name, id)| (name.clone(), id.0)).collect();
            for row in &mut pending.rows {
                if row.media.is_empty() {
                    continue;
                }
                if let Some(h) = &row.front_html {
                    row.front_html = Some(flash_store::richtext::activate_media_refs(h, &resolve));
                }
                if let Some(h) = &row.back_html {
                    row.back_html = Some(flash_store::richtext::activate_media_refs(h, &resolve));
                }
            }
        }
    }

    // The clean-look choice subtracts the remapped color classes the
    // parser baked in at preview time.
    if opts.clean_colors {
        for row in &mut pending.rows {
            if let Some(h) = &row.front_html {
                row.front_html = Some(flash_store::richtext::strip_highlight_classes(h));
            }
            if let Some(h) = &row.back_html {
                row.back_html = Some(flash_store::richtext::strip_highlight_classes(h));
            }
        }
    }

    let ingest_ms = commit_started.elapsed().as_millis() as u64;
    let insert_started = std::time::Instant::now();
    // The service admits or rejects the whole batch in one decision
    // (and starts the one-time overflow grace on oversized imports).
    let total = services.import_cards(
        user,
        &pending.default_deck,
        pending.rows,
        opts.progress,
        media_map.as_ref(),
        now_ms(),
    )?;
    tracing::info!(
        user = user.raw(),
        cards = total,
        media = media_map.as_ref().map_or(0, |m| m.len()),
        with_progress = opts.progress,
        ingest_ms,
        insert_ms = insert_started.elapsed().as_millis() as u64,
        "import committed"
    );
    if opts.settings {
        if let Some(s) = &pending.settings {
            if let Err(e) = services.adopt_imported_settings(user, s) {
                tracing::warn!("adopt imported settings: {e}");
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&apkg_path);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::common_deck_parent;

    #[test]
    fn shared_parent_or_first() {
        let d = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            common_deck_parent(&d(&["Pharm::Cardio", "Pharm::Renal"])),
            "Pharm"
        );
        assert_eq!(
            common_deck_parent(&d(&["A::B::C", "A::B::D", "A::B"])),
            "A::B"
        );
        assert_eq!(common_deck_parent(&d(&["Cardio", "Renal"])), "Cardio");
    }
}
