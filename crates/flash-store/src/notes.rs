//! Note types and the pure note -> cards expansion the editor relies on.
//! A note is the editable unit (Front/Back, or cloze Text/Extra); cards
//! are what the scheduler sees. The expansion here mirrors the .apkg
//! importer exactly, so a card authored in the editor is indistinguishable
//! from one imported from Anki: same plain text, same precomputed cloze
//! views, same export grouping.

use flash_core::{validate_card_text, CardText};

use crate::import::cloze;
use crate::repo::cards::CardExtras;
use crate::richtext::{self, SanitizedHtml};

/// Ceiling on one field's HTML (matches the importer's rich-side cap).
pub const MAX_FIELD_HTML: usize = 40_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteType {
    Basic,
    BasicReversed,
    BasicTyped,
    Cloze,
}

impl NoteType {
    pub const ALL: [NoteType; 4] = [
        NoteType::Basic,
        NoteType::BasicReversed,
        NoteType::BasicTyped,
        NoteType::Cloze,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NoteType::Basic => "basic",
            NoteType::BasicReversed => "basic_reversed",
            NoteType::BasicTyped => "basic_typed",
            NoteType::Cloze => "cloze",
        }
    }

    pub fn parse(s: &str) -> Option<NoteType> {
        NoteType::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// Human label, as shown in the editor's type picker.
    pub fn label(self) -> &'static str {
        match self {
            NoteType::Basic => "Basic",
            NoteType::BasicReversed => "Basic (and reversed card)",
            NoteType::BasicTyped => "Basic (type in the answer)",
            NoteType::Cloze => "Cloze",
        }
    }
}

/// One card a note expands to. `ord` is the stable slot within the note
/// (reversed: 0 front->back, 1 back->front; cloze: index - 1).
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedCard {
    pub ord: u32,
    pub text: CardText,
    pub extras: CardExtras,
}

/// Rich HTML worth storing, or None for a plain side (same rule as the
/// importer: markup-free content stays a plain card).
fn rich(html: &SanitizedHtml) -> Option<SanitizedHtml> {
    if html.trim().contains('<') {
        Some(html.clone().trimmed())
    } else {
        None
    }
}

fn side(html: &str) -> String {
    richtext::html_to_text(html)
}

/// Expands sanitized note fields into cards. Errors are user-facing. The
/// fields arrive as `SanitizedHtml`, so a caller cannot hand the
/// expansion raw editor input.
pub fn generate_cards(
    kind: NoteType,
    front_html: &SanitizedHtml,
    back_html: &SanitizedHtml,
) -> Result<Vec<GeneratedCard>, String> {
    if front_html.len() > MAX_FIELD_HTML || back_html.len() > MAX_FIELD_HTML {
        return Err(format!("a field exceeds {MAX_FIELD_HTML} bytes of HTML"));
    }
    match kind {
        NoteType::Basic | NoteType::BasicReversed | NoteType::BasicTyped => {
            let front = side(front_html);
            let back = side(back_html);
            let text = validate_card_text(&front, &back)?;
            let mut out = vec![GeneratedCard {
                ord: 0,
                text: text.clone(),
                extras: CardExtras {
                    front_html: rich(front_html),
                    back_html: rich(back_html),
                    type_answer: (kind == NoteType::BasicTyped).then(|| text.back.clone()),
                    ..CardExtras::default()
                },
            }];
            if kind == NoteType::BasicReversed {
                out.push(GeneratedCard {
                    ord: 1,
                    text: CardText {
                        front: text.back.clone(),
                        back: text.front.clone(),
                    },
                    extras: CardExtras {
                        front_html: rich(back_html),
                        back_html: rich(front_html),
                        ..CardExtras::default()
                    },
                });
            }
            Ok(out)
        }
        NoteType::Cloze => {
            let cz = cloze::parse(front_html)
                .ok_or_else(|| "add at least one {{c1::…}} cloze deletion".to_string())?;
            let text_plain = side(front_html);
            let extra_plain = side(back_html);
            let source = format!("{text_plain}\u{1f}{extra_plain}");
            let mut out = Vec::new();
            for idx in cz.indices() {
                let front = side(&cz.front(idx, false));
                let front_html = richtext::sanitize_with_media(&cz.front(idx, true));
                let answers = cz.answers(idx).join("; ");
                let answer_text = side(&answers);
                if answer_text.is_empty() {
                    return Err(format!("cloze c{idx} has an empty answer"));
                }
                let back = if extra_plain.is_empty() {
                    answer_text
                } else {
                    format!("{answer_text}\n{extra_plain}")
                };
                let filled = cz.filled(idx);
                let back_html = richtext::sanitize_with_media(&if back_html.trim().is_empty() {
                    filled
                } else {
                    format!("{filled}<br>{back_html}")
                });
                let text =
                    validate_card_text(&front, &back).map_err(|e| format!("cloze c{idx}: {e}"))?;
                out.push(GeneratedCard {
                    ord: idx - 1,
                    text,
                    extras: CardExtras {
                        front_html: Some(front_html),
                        back_html: Some(back_html),
                        cloze_text: Some(source.clone()),
                        cloze_index: Some(idx),
                        type_answer: None,
                    },
                });
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests write fields as strings; the editor path sanitizes them
    /// before generation, so the tests do the same here.
    fn generate_cards(
        kind: NoteType,
        front: &str,
        back: &str,
    ) -> Result<Vec<GeneratedCard>, String> {
        super::generate_cards(
            kind,
            &richtext::sanitize_with_media(front),
            &richtext::sanitize_with_media(back),
        )
    }

    #[test]
    fn note_type_round_trips() {
        for t in NoteType::ALL {
            assert_eq!(NoteType::parse(t.as_str()), Some(t));
        }
        assert_eq!(NoteType::parse("nope"), None);
    }

    #[test]
    fn basic_plain_and_rich() {
        let cards = generate_cards(NoteType::Basic, "Hello", "World").unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].text.front, "Hello");
        assert!(
            cards[0].extras.front_html.is_none(),
            "no markup = plain card"
        );

        let cards = generate_cards(NoteType::Basic, "<b>Hi</b> there", "a<br>b").unwrap();
        assert_eq!(cards[0].text.front, "Hi there");
        assert_eq!(cards[0].text.back, "a\nb");
        assert_eq!(
            cards[0].extras.front_html.as_deref(),
            Some("<b>Hi</b> there")
        );
        assert_eq!(cards[0].extras.back_html.as_deref(), Some("a<br>b"));
    }

    #[test]
    fn reversed_swaps_sides_and_html() {
        let cards = generate_cards(NoteType::BasicReversed, "<i>Q</i>", "A").unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!((cards[0].ord, cards[1].ord), (0, 1));
        assert_eq!(cards[1].text.front, "A");
        assert_eq!(cards[1].text.back, "Q");
        assert_eq!(cards[1].extras.back_html.as_deref(), Some("<i>Q</i>"));
        assert!(cards[1].extras.front_html.is_none());
    }

    #[test]
    fn typed_sets_answer() {
        let cards = generate_cards(NoteType::BasicTyped, "Q", "<b>Ans</b>").unwrap();
        assert_eq!(cards[0].extras.type_answer.as_deref(), Some("Ans"));
    }

    #[test]
    fn cloze_fans_out_per_index() {
        let cards = generate_cards(
            NoteType::Cloze,
            "{{c1::Paris}} is the capital of {{c2::France::country}}",
            "<i>Geo</i>",
        )
        .unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].ord, 0);
        assert_eq!(cards[0].extras.cloze_index, Some(1));
        assert_eq!(cards[0].text.front, "[...] is the capital of France");
        assert_eq!(cards[0].text.back, "Paris\nGeo");
        let fh = cards[0].extras.front_html.as_deref().unwrap();
        assert!(fh.contains("cloze-blank"), "{fh}");
        let bh = cards[0].extras.back_html.as_deref().unwrap();
        assert!(
            bh.contains("cloze-answer") && bh.contains("<i>Geo</i>"),
            "{bh}"
        );
        assert_eq!(cards[1].text.front, "Paris is the capital of [country]");
        assert_eq!(
            cards[1].extras.cloze_text.as_deref(),
            Some("{{c1::Paris}} is the capital of {{c2::France::country}}\u{1f}Geo")
        );
        assert_eq!(cards[1].extras.cloze_text, cards[0].extras.cloze_text);
    }

    #[test]
    fn cloze_without_markup_is_rejected() {
        let err = generate_cards(NoteType::Cloze, "no blanks here", "").unwrap_err();
        assert!(err.contains("c1"), "{err}");
    }

    #[test]
    fn cloze_empty_answer_is_rejected() {
        let err = generate_cards(NoteType::Cloze, "x {{c1::}} y", "").unwrap_err();
        assert!(err.contains("empty") || err.contains("c1"), "{err}");
    }

    #[test]
    fn empty_sides_are_rejected() {
        assert!(generate_cards(NoteType::Basic, "", "b").is_err());
        assert!(generate_cards(NoteType::Basic, "<br>", "b").is_err());
        assert!(generate_cards(NoteType::Basic, "a", " ").is_err());
    }

    #[test]
    fn media_only_side_gets_a_label() {
        let cards = generate_cards(NoteType::Basic, "<img src=\"/media/7\">", "b").unwrap();
        assert_eq!(cards[0].text.front, "[image]");
        assert_eq!(
            richtext::media_ids(cards[0].extras.front_html.as_deref().unwrap()),
            vec![7]
        );
    }
}
