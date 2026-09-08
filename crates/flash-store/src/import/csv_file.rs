//! CSV import: header `front,back,tags,deck`; tags semicolon- or
//! comma-separated; tags/deck columns optional.

use super::{ImportError, ImportRow, ParsedImport};

pub fn parse_csv(bytes: &[u8]) -> Result<ParsedImport, ImportError> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .map_err(|e| ImportError::new("the CSV's header row could not be read", e))?
        .clone();
    let col = |name: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(name));
    let (Some(front_col), Some(back_col)) = (col("front"), col("back")) else {
        return Err(ImportError::user(
            "CSV needs 'front' and 'back' header columns",
        ));
    };
    let tags_col = col("tags");
    let deck_col = col("deck");

    let mut out = ParsedImport::default();
    // A message per bad row, up to a point: a file of nothing but bad
    // rows reports the first hundred and counts the rest.
    let note = |out: &mut ParsedImport, message: String| {
        if out.messages.len() < super::MAX_MESSAGES {
            out.messages.push(message);
        }
    };
    for (i, record) in reader.records().enumerate() {
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                out.skipped += 1;
                note(&mut out, format!("row {}: {e}", i + 2));
                continue;
            }
        };
        let front = super::clip_raw(record.get(front_col).unwrap_or("").trim());
        let back = super::clip_raw(record.get(back_col).unwrap_or("").trim());
        if front.is_empty() || back.is_empty() {
            out.skipped += 1;
            note(&mut out, format!("row {}: empty front or back", i + 2));
            continue;
        }
        let tags = tags_col
            .and_then(|c| record.get(c))
            .map(|s| {
                super::clip_tags(
                    s.split([';', ','])
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string),
                )
            })
            .unwrap_or_default();
        let deck = deck_col
            .and_then(|c| record.get(c))
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        out.push_row(ImportRow {
            front: front.to_string(),
            back: back.to_string(),
            tags,
            deck,
            reviews: Vec::new(),
            media: Vec::new(),
            front_html: None,
            back_html: None,
            suspended: false,
            cloze_text: None,
            cloze_index: None,
            type_answer: None,
        })?;
    }
    Ok(out)
}
