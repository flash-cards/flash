//! File imports. Parsers produce ImportRows; committing them goes through
//! the normal create_cards path so imported cards are indistinguishable
//! from hand-made ones. Anki scheduling state is deliberately not imported:
//! FSRS starts fresh and converges from real reviews.

pub(crate) mod apkg;
mod apkg_media;
pub(crate) mod cloze;
mod colorize;
mod csv_file;
mod deck_options;
mod notetype;
mod patterns;
mod proto;
mod template;

pub use apkg::parse_apkg;
pub use apkg_media::{extract_media_file, read_media_manifest, ApkgMediaEntry};
pub use colorize::{parse_css_class_colors, remap_colors, ClassColorMap};
pub use csv_file::parse_csv;
pub use deck_options::ImportedSettings;

use crate::media::MediaRef;
use crate::richtext::SanitizedHtml;

/// Most cards one file may yield. The parsers stop reading at this
/// many rows and refuse the file, so a collection of millions of tiny
/// notes is a sentence rather than a Vec the size of memory.
pub const MAX_ROWS: usize = 50_000;

/// Most per-row messages a parse keeps (a file of nothing but bad rows
/// would otherwise report each one).
pub const MAX_MESSAGES: usize = 100;

/// Longest raw field the rewriters see, in bytes: a small multiple of
/// the rich side the store keeps, since nothing past it can survive
/// into a card, and the pre-sanitize rewriters are not linear. Every
/// pipeline clips its input to this on entry, so the bound holds
/// whatever produced the text: a field, a rendered template, a cloze
/// expansion.
pub const MAX_RAW_FIELD_BYTES: usize = 4 * crate::notes::MAX_FIELD_HTML;

/// Most fields one note may carry. Anki notetypes have a handful; the
/// cap bounds what a single `flds` cell can make the parser scan.
pub const MAX_FIELDS_PER_NOTE: usize = 64;

/// Most rows a lookup table of the collection (decks, notetypes, their
/// fields and templates, deck options) contributes. Real collections
/// have dozens; every read of one carries this as its LIMIT.
pub(crate) const MAX_LOOKUP_ROWS: usize = 10_000;

/// Most entries an .apkg may hold: a deck's media plus a few metadata
/// files. The zip central directory is parsed in memory, so a package
/// of a million empty entries would otherwise cost a gigabyte before
/// the first byte of content is read.
pub const MAX_PACKAGE_ENTRIES: usize = 20_000;

/// The sentence a file over `MAX_ROWS` gets.
pub fn too_many_cards() -> ImportError {
    ImportError::user(format!(
        "that file has more than {MAX_ROWS} cards; imports are capped at {MAX_ROWS} per file — split it in Anki first"
    ))
}

/// `s` cut to at most `MAX_RAW_FIELD_BYTES` on a character boundary.
pub(crate) fn clip_raw(s: &str) -> &str {
    if s.len() <= MAX_RAW_FIELD_BYTES {
        return s;
    }
    let mut end = MAX_RAW_FIELD_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` cut in place to at most `MAX_RAW_FIELD_BYTES` on a character
/// boundary; true when it was cut.
pub(crate) fn clip_in_place(s: &mut String) -> bool {
    if s.len() <= MAX_RAW_FIELD_BYTES {
        return false;
    }
    let end = clip_raw(s).len();
    s.truncate(end);
    true
}

/// A row's tags as the store will accept them: at most
/// `MAX_TAGS_PER_CARD`, each at most `MAX_TAG_LEN` bytes; the rest are
/// dropped rather than failing the whole file.
pub(crate) fn clip_tags(tags: impl IntoIterator<Item = String>) -> Vec<String> {
    tags.into_iter()
        .filter(|t| t.len() <= crate::repo::cards::MAX_TAG_LEN)
        .take(crate::repo::cards::MAX_TAGS_PER_CARD)
        .collect()
}

/// The private decoders, for the fuzz targets only: a fuzzer that
/// starts from raw bytes never synthesizes the zip, zstd and SQLite
/// layers around them, so it fuzzes them directly.
#[cfg(feature = "fuzzing")]
pub mod fuzz {
    /// The protobuf media manifest; how many entries it yielded.
    pub fn media_entries_proto(bytes: &[u8]) -> usize {
        super::apkg_media::parse_media_entries_proto(bytes).len()
    }

    /// The legacy JSON media manifest.
    pub fn media_map_json(bytes: &[u8]) -> bool {
        super::apkg_media::parse_media_map_json(bytes).is_ok()
    }

    /// A notetype's CSS and its templates, from its config blob.
    pub fn notetype_config(bytes: &[u8]) -> String {
        let _ = super::notetype::template_formats(bytes);
        super::notetype::notetype_css(bytes)
    }

    /// A deck's options blob.
    pub fn deck_config(bytes: &[u8]) -> super::ImportedSettings {
        super::deck_options::config_settings(bytes)
    }

    /// Walks `bytes` as a protobuf message with the primitives the
    /// decoders share, reading each field every way they might.
    pub fn walk_fields(bytes: &[u8]) {
        use super::proto::{packed_floats, read_bytes, read_f32, read_varint, skip_field};
        let mut i = 0;
        while i < bytes.len() {
            let before = i;
            let Some(tag) = read_varint(bytes, &mut i) else {
                break;
            };
            let mut probe = i;
            if let Some(payload) = read_bytes(bytes, &mut probe) {
                let _ = packed_floats(payload);
            }
            let _ = read_f32(bytes, &mut i.clone());
            if !skip_field(bytes, &mut i, tag & 7) || i <= before {
                break;
            }
        }
    }
}

/// Why an upload could not be parsed, in two voices: `public` is the
/// sentence the uploader sees, `detail` is what the library said, for
/// the log. The split is the type's whole point: a caller cannot show
/// the detail by accident, because `Display` is the public sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportError {
    public: String,
    detail: String,
}

impl ImportError {
    /// A failure with something worth logging behind it.
    pub fn new(public: impl Into<String>, detail: impl std::fmt::Display) -> Self {
        Self {
            public: public.into(),
            detail: detail.to_string(),
        }
    }

    /// A failure that is entirely the file's own doing; nothing to log.
    pub fn user(public: impl Into<String>) -> Self {
        Self {
            public: public.into(),
            detail: String::new(),
        }
    }

    pub fn public(&self) -> &str {
        &self.public
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.public)
    }
}

impl std::error::Error for ImportError {}

/// One card ready to import.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportRow {
    /// Anki review history for this card, oldest first, capped to the most
    /// recent entries: (reviewed_at_ms, rating 1-4). Empty for CSV imports
    /// or decks without a revlog. Used (opt-in) to reconstruct scheduling
    /// state by replaying through FSRS.
    #[serde(default)]
    pub reviews: Vec<(i64, u8)>,
    /// Media files this card's fields reference (from the raw Anki HTML,
    /// before tag stripping). Empty for CSV imports.
    #[serde(default)]
    pub media: Vec<MediaRef>,
    pub front: String,
    pub back: String,
    /// Sanitized rich HTML for the web UI; None = plain card (the text is
    /// the whole content). MCP/voice always use the plain text. Typed:
    /// a parked preview read back from disk is re-sanitized as it loads.
    #[serde(default)]
    pub front_html: Option<SanitizedHtml>,
    #[serde(default)]
    pub back_html: Option<SanitizedHtml>,
    pub tags: Vec<String>,
    /// Deck named by the file itself; falls back to the user's choice.
    pub deck: Option<String>,
    /// Suspended in Anki (queue -1): imported paused, exactly as it was.
    #[serde(default)]
    pub suspended: bool,
    /// For cloze-derived cards: the original `Text\x1fExtra` source with
    /// markup intact (round-trip export) and which index this card is.
    #[serde(default)]
    pub cloze_text: Option<String>,
    #[serde(default)]
    pub cloze_index: Option<u32>,
    /// The expected answer when the Anki template asked to type it
    /// ({{type:Field}}); drives the interactive typing UI in web study.
    #[serde(default)]
    pub type_answer: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ParsedImport {
    /// Filled only through `push_row`, which carries the cap: no
    /// parser path can grow this past `MAX_ROWS`, whether it makes one
    /// row per note, per card, or per cloze index.
    pub rows: Vec<ImportRow>,
    /// Rows skipped (empty sides, unsupported notetypes, ...).
    pub skipped: u32,
    /// Human-readable notes for the preview screen.
    pub messages: Vec<String>,
    /// What media the package's cards reference (deduped across cards).
    #[serde(default)]
    pub media: MediaSummary,
    /// FSRS-relevant deck options found in the package (offered opt-in).
    #[serde(default)]
    pub settings: Option<ImportedSettings>,
}

impl ParsedImport {
    /// Adds a row, or refuses the file once it holds `MAX_ROWS`: the
    /// check sits on the push itself, so a file past the cap costs at
    /// most one row over, never a Vec the size of memory.
    pub(crate) fn push_row(&mut self, row: ImportRow) -> Result<(), ImportError> {
        if self.rows.len() >= MAX_ROWS {
            return Err(too_many_cards());
        }
        self.rows.push(row);
        Ok(())
    }
}

/// Media referenced by imported cards, tallied for the preview screen and
/// (later) the media-import decision.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaSummary {
    pub images: u32,
    pub audio: u32,
    pub video: u32,
    /// Referenced but not a format Flash accepts (e.g. SVG).
    pub unsupported: u32,
    /// Referenced by a card but absent from the package.
    pub missing: u32,
    /// Total bytes of the referenced files present in the package.
    pub total_bytes: u64,
}

impl MediaSummary {
    pub fn referenced(&self) -> u32 {
        self.images + self.audio + self.video + self.unsupported
    }
}
