//! Minimal Anki card-template renderer: enough of the mustache dialect to
//! reproduce what each card actually shows — `{{Field}}` substitution,
//! filters (`{{furigana:Reading}}`, `{{tts ja_JP:Word}}`, …: the field is
//! the last `:`-segment; unknown filters just render the field),
//! `{{#F}}/{{^F}}/{{/F}}` conditionals (nested), and the specials.
//! `{{FrontSide}}` renders empty: Flash always shows the front above the
//! back, so echoing it would be duplication, not fidelity.

use std::collections::HashMap;

/// Renders a card format. The output is capped at `MAX_RAW_FIELD_BYTES`
/// like a raw field: the fields are clipped, but a template may name
/// one of them any number of times, and a render is what the pipelines
/// see next.
pub fn render(fmt: &str, fields: &HashMap<&str, &str>, is_back: bool) -> String {
    let mut out = String::new();
    let mut stack: Vec<bool> = Vec::new();
    // Fields already shown as text in this format — lets {{tts:X}} render
    // silently when X is visible anyway (Anki shows a play button there,
    // not the text twice).
    let mut shown: Vec<&str> = Vec::new();
    let mut rest = fmt;
    loop {
        if super::clip_in_place(&mut out) {
            return out;
        }
        let emitting = stack.iter().all(|b| *b);
        let Some(start) = rest.find("{{") else {
            if emitting {
                out.push_str(rest);
            }
            super::clip_in_place(&mut out);
            return out;
        };
        if emitting {
            out.push_str(&rest[..start]);
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // Unclosed marker: literal.
            if emitting {
                out.push_str(&rest[start..]);
            }
            super::clip_in_place(&mut out);
            return out;
        };
        let token = after[..end].trim();
        rest = &after[end + 2..];
        if let Some(name) = token.strip_prefix('#') {
            stack.push(!field_value(fields, name.trim()).trim().is_empty());
        } else if let Some(name) = token.strip_prefix('^') {
            stack.push(field_value(fields, name.trim()).trim().is_empty());
        } else if token.starts_with('/') {
            stack.pop();
        } else if emitting {
            out.push_str(&substitute(token, fields, is_back, &mut shown));
        }
    }
}

/// Whether this question format asks the user to type an answer, and
/// which field it compares against.
pub fn type_answer_field(qfmt: &str) -> Option<&str> {
    let start = qfmt.find("{{type:")?;
    let inner = &qfmt[start + 7..qfmt[start..].find("}}")? + start];
    let name = inner.split(':').next_back().unwrap_or(inner).trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn field_value<'a>(fields: &HashMap<&str, &'a str>, name: &str) -> &'a str {
    fields.get(name).copied().unwrap_or("")
}

fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn substitute<'a>(
    token: &'a str,
    fields: &HashMap<&str, &str>,
    is_back: bool,
    shown: &mut Vec<&'a str>,
) -> String {
    let name = token.split(':').next_back().unwrap_or(token).trim();
    match name {
        // Flash renders front and back stacked; FrontSide would duplicate.
        "FrontSide" => return String::new(),
        "Tags" | "Type" | "Deck" | "Subdeck" | "Card" | "CardFlag" | "Flags" => {
            return String::new()
        }
        _ => {}
    }
    let filters: Vec<&str> = token.split(':').collect();
    let filters = &filters[..filters.len().saturating_sub(1)];
    if filters.iter().any(|f| f.trim() == "type") && !is_back {
        // The typing prompt itself; the interactive input is the UI's job.
        return String::new();
    }
    if filters.iter().any(|f| f.trim() == "hint") {
        // Anki renders a collapsed "Show <Field>" link; the native
        // equivalent is a <details> the sanitizer admits. Empty field ->
        // nothing, matching Anki. The content is not marked `shown`: it
        // stays hidden until opened.
        let value = field_value(fields, name);
        if value.trim().is_empty() {
            return String::new();
        }
        return format!(
            "<details><summary>{}</summary>{value}</details>",
            esc_text(name)
        );
    }
    if filters.iter().any(|f| f.trim().starts_with("tts")) {
        // In Anki this is a play button. If the field is already visible
        // in this format, showing its text again is duplication; a
        // tts-only card keeps the text (voice mode needs something to say).
        if shown.contains(&name) {
            return String::new();
        }
        shown.push(name);
        return field_value(fields, name).to_string();
    }
    shown.push(name);
    field_value(fields, name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn substitution_and_frontside() {
        let f = fields(&[
            ("Word", "食べる"),
            ("Reading", "たべる"),
            ("Meaning", "to eat"),
        ]);
        assert_eq!(
            render("{{Word}} ({{Reading}})", &f, false),
            "食べる (たべる)"
        );
        assert_eq!(
            render("{{FrontSide}}<hr>{{Meaning}}", &f, true),
            "<hr>to eat"
        );
    }

    #[test]
    fn filters_take_last_segment() {
        let f = fields(&[("Reading", "たべる"), ("Word", "食べる")]);
        assert_eq!(render("{{furigana:Reading}}", &f, false), "たべる");
        assert_eq!(
            render("{{tts ja_JP voices=Apple:Word}}", &f, false),
            "食べる"
        );
        assert_eq!(render("{{unknown_filter:Word}}", &f, true), "食べる");
    }

    #[test]
    fn hint_filter_renders_collapsible() {
        let f = fields(&[("Extra", "<b>mnemonic</b>"), ("Empty", " ")]);
        assert_eq!(
            render("{{hint:Extra}}", &f, true),
            "<details><summary>Extra</summary><b>mnemonic</b></details>"
        );
        // Empty field: Anki hides the hint link entirely.
        assert_eq!(render("x{{hint:Empty}}", &f, true), "x");
        // Other unknown filters still fall through to the plain field.
        assert_eq!(
            render("{{unknown_filter:Extra}}", &f, true),
            "<b>mnemonic</b>"
        );
    }

    #[test]
    fn tts_renders_silently_when_its_field_is_already_visible() {
        let f = fields(&[("Word", "食べる"), ("Reading", "たべる")]);
        // Word is shown, then spoken: the tts token adds nothing.
        assert_eq!(
            render(
                "{{Word}} ({{Reading}}) {{tts ja_JP voices=Any:Word}}",
                &f,
                false
            ),
            "食べる (たべる) "
        );
        // A tts-only card keeps its text — voice needs something to say.
        assert_eq!(render("{{tts ja_JP:Word}}", &f, false), "食べる");
    }

    #[test]
    fn type_filter_hides_on_front_shows_on_back() {
        let f = fields(&[("Answer", "42")]);
        assert_eq!(render("Q: {{type:Answer}}", &f, false), "Q: ");
        assert_eq!(render("A: {{type:Answer}}", &f, true), "A: 42");
        assert_eq!(type_answer_field("Q {{type:Answer}}"), Some("Answer"));
        assert_eq!(type_answer_field("plain {{Field}}"), None);
    }

    #[test]
    fn conditionals_nested() {
        let f = fields(&[("A", "yes"), ("B", "")]);
        assert_eq!(
            render(
                "{{#A}}a={{A}}{{#B}} b={{B}}{{/B}}{{/A}}{{^B}} nob{{/B}}",
                &f,
                false
            ),
            "a=yes nob"
        );
        assert_eq!(render("{{#B}}hidden{{/B}}visible", &f, false), "visible");
    }

    #[test]
    fn unclosed_marker_is_literal() {
        let f = fields(&[("A", "x")]);
        assert_eq!(
            render("start {{A}} then {{broken", &f, false),
            "start x then {{broken"
        );
    }

    #[test]
    fn unknown_fields_render_empty() {
        let f = fields(&[]);
        assert_eq!(render("[{{Missing}}]", &f, false), "[]");
    }
}
