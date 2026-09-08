//! Export: the user's cards (and their media) as an Anki .apkg, or CSV. Never gated — "your
//! cards are always yours" is a product invariant. Scheduling state is
//! deliberately not exported, symmetric with the import philosophy (FSRS
//! state has no faithful SM-2 mapping); cards arrive elsewhere as new.

mod apkg;

pub use apkg::{build_apkg, build_apkg_into, rewrite_media_refs};

/// One media file the package will carry: its name inside the package
/// and how to fetch it. No bytes: the builder asks for each entry's
/// content right before writing it and drops it right after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportMediaEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// What an .apkg bundles, decided before any byte moves: the entries in
/// package order, and `names` mapping a media row id to the filename the
/// card HTML is rewritten to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportMediaPlan {
    pub entries: Vec<ExportMediaEntry>,
    pub names: std::collections::HashMap<i64, String>,
}

/// Media blobs to bundle into an .apkg, already in memory: `names` maps a
/// media row id to the filename used inside the package; `files` are
/// (filename, bytes) in that same naming. The server plans and streams
/// instead (`ExportMediaPlan`); this form serves tests and small callers.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ExportMedia {
    pub files: Vec<(String, Vec<u8>)>,
    pub names: std::collections::HashMap<i64, String>,
}

impl ExportMedia {
    /// The same package, described without its bytes.
    pub fn plan(&self) -> ExportMediaPlan {
        ExportMediaPlan {
            entries: self
                .files
                .iter()
                .map(|(name, bytes)| ExportMediaEntry {
                    name: name.clone(),
                    sha256: String::new(),
                    size: bytes.len() as u64,
                })
                .collect(),
            names: self.names.clone(),
        }
    }
}

/// One exportable card, deck name resolved.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExportCard {
    pub deck: String,
    pub front: String,
    pub back: String,
    /// Sanitized rich HTML (imported formatting); .apkg export prefers it
    /// so structure survives the round trip. CSV always uses plain text.
    pub front_html: Option<String>,
    pub back_html: Option<String>,
    /// Cloze-derived cards: (original `Text\x1fExtra` source, index).
    /// Cards sharing a source re-group into one real cloze note on export.
    pub cloze: Option<(String, u32)>,
    /// Type-in-the-answer cards export with a typed template.
    pub type_answer: Option<String>,
    pub tags: Vec<String>,
}

/// RFC 4180 CSV with the same header the importer accepts, so a Flash
/// export re-imports into Flash losslessly. Tags are `;`-joined — exactly
/// what the importer splits on.
pub fn build_csv(cards: &[ExportCard]) -> String {
    let mut out = String::from("front,back,deck,tags\r\n");
    for card in cards {
        for (i, field) in [
            card.front.as_str(),
            card.back.as_str(),
            card.deck.as_str(),
            &card.tags.join(";"),
        ]
        .iter()
        .enumerate()
        {
            if i > 0 {
                out.push(',');
            }
            // Spreadsheets execute cells that start like formulas, and a
            // card's text is not always the exporter's own words (copied
            // community decks, MCP clients). A leading apostrophe makes
            // the cell literal text in Excel, Sheets and LibreOffice; the
            // importer's trim never sees it because the cell is quoted.
            let formula_like = field.starts_with(['=', '+', '-', '@', '\t', '\r']);
            if formula_like || field.contains(['"', ',', '\n', '\r']) {
                out.push('"');
                if formula_like {
                    out.push('\'');
                }
                out.push_str(&field.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(field);
            }
        }
        out.push_str("\r\n");
    }
    out
}
