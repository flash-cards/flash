//! Rich card content: the sanitized-HTML side of the dual representation.
//! Imported cards keep their *semantic* structure (bold, lists, tables,
//! sub/sup) for the web UI while every scrap of imported styling — style
//! attributes, font/color tags, classes — is stripped: rich cards render
//! entirely in the Flash Design System, theme-aware. Plain text for
//! MCP/voice/CSV is derived separately (import pipeline).
//!
//! Security: this HTML goes into our pages with askama `|safe`, so the
//! sanitizer is the wall. Ammonia parses with a real HTML5 parser and
//! rebuilds only allowlisted structure; scripts/handlers/URLs cannot
//! survive. CSP (`script-src 'self'`) backs it up.

use std::collections::{HashMap, HashSet};

use crate::media::{classify, tag_name, MediaKind};

/// HTML that has been through this module's sanitizer and nothing since.
///
/// The field is private and the only constructors are the sanitizers in
/// this module, so a value of this type is proof that the bytes came out
/// of ammonia last. Every store column that holds rich HTML accepts only
/// this type; a rewriter that takes sanitized HTML apart (the highlight
/// stripper, the media activator, the id remapper) hands its result back
/// through a sanitizer before returning, and any new rewriter that does
/// not cannot store what it made. The 2026-09-06 review found exactly
/// that shape: a class smuggled inside another attribute's value was
/// re-emitted as a real class by a rewriter and stored as-is.
///
/// Deserializing (the parked import preview, a share snapshot) sanitizes
/// on the way in: the file is ours, but the type's promise does not rest
/// on that.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct SanitizedHtml(String);

impl SanitizedHtml {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Leading and trailing whitespace removed. Whitespace around
    /// sanitized markup is still sanitized markup.
    pub fn trimmed(self) -> Self {
        Self(self.0.trim().to_string())
    }
}

impl std::ops::Deref for SanitizedHtml {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SanitizedHtml {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SanitizedHtml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for SanitizedHtml {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SanitizedHtml {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for SanitizedHtml {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl<'de> serde::Deserialize<'de> for SanitizedHtml {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(sanitize_with_media(&raw))
    }
}

impl rusqlite::ToSql for SanitizedHtml {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

/// Ids of the images a sanitized card side embeds, in order, deduped.
/// The sanitizer guarantees `src` is exactly `/media/<digits>` and the
/// only tags carrying it are img/audio/video; only images are returned
/// (players stay `preload="none"`). Used to preload a back's pictures
/// while the front is on screen.
pub fn media_image_ids(html: &str) -> Vec<i64> {
    const MARK: &str = "src=\"/media/";
    let mut out: Vec<i64> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        let is_img = rest[..at]
            .rfind('<')
            .is_some_and(|lt| rest[lt + 1..].starts_with("img"));
        let digits: String = rest[at + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if is_img {
            if let Ok(id) = digits.parse::<i64>() {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        rest = &rest[at + MARK.len()..];
    }
    out
}

/// Ids of every media element (img/audio/video) a sanitized side embeds,
/// in order, deduped — what a card must be linked to so orphan GC keeps
/// the blobs alive.
pub fn media_ids(html: &str) -> Vec<i64> {
    const MARK: &str = "src=\"/media/";
    let mut out: Vec<i64> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        let digits: String = rest[at + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(id) = digits.parse::<i64>() {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        rest = &rest[at + MARK.len()..];
    }
    out
}

/// Rewrites every `/media/<id>` reference through `map` (ids without a
/// mapping are left alone) and re-sanitizes under the media profile —
/// the step that moves sanitized HTML between id spaces (a share
/// snapshot, a copied deck) without loosening the sanitizer's wall.
pub fn remap_media_ids(html: &str, map: &std::collections::HashMap<i64, i64>) -> SanitizedHtml {
    const NEEDLE: &str = "/media/";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(NEEDLE) {
        let after = &rest[at + NEEDLE.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        out.push_str(&rest[..at + NEEDLE.len()]);
        match digits.parse::<i64>().ok().and_then(|id| map.get(&id)) {
            Some(new_id) => out.push_str(&new_id.to_string()),
            None => out.push_str(&digits),
        }
        rest = &after[digits.len()..];
    }
    out.push_str(rest);
    sanitize_with_media(&out)
}

/// Editor HTML -> the plain text every non-web surface (MCP, voice, CSV,
/// search) uses. Same pipeline the importer runs on Anki fields, so
/// hand-authored and imported cards read identically to an assistant.
/// A side that is nothing but media gets a speakable kind label.
pub fn html_to_text(html: &str) -> String {
    let text = crate::import::apkg::text_pipeline(html);
    if !text.is_empty() {
        return text;
    }
    let lower = html.to_ascii_lowercase();
    let mut labels: Vec<&str> = Vec::new();
    for (tag, label) in [
        ("<img", "[image]"),
        ("<audio", "[audio]"),
        ("<video", "[video]"),
    ] {
        if lower.contains(tag) {
            labels.push(label);
        }
    }
    labels.join(" ")
}

/// Classes our own pipeline emits and the UI styles; the only class
/// values allowed to survive sanitization (plus `hl-*` highlight classes,
/// validated by `is_highlight_class`).
const ALLOWED_CLASSES: &[&str] = &[
    "media-ref",
    "cloze-blank",
    "cloze-answer",
    "cloze-answer-block",
];

/// The color-remap hues the design system defines (import/colorize.rs
/// emits them; app.css styles them per theme).
pub const HIGHLIGHT_HUES: &[&str] = &["red", "amber", "green", "teal", "blue", "purple", "pink"];

/// `hl-{hue}` (text color) or `hl-bg-{hue}` (background tint).
fn is_highlight_class(token: &str) -> bool {
    let rest = match token.strip_prefix("hl-") {
        Some(r) => r,
        None => return false,
    };
    let hue = rest.strip_prefix("bg-").unwrap_or(rest);
    HIGHLIGHT_HUES.contains(&hue)
}

/// Sanitizes card HTML to semantic structure only. Everything
/// presentational is removed; text content of stripped tags survives.
pub fn sanitize_card_html(html: &str) -> SanitizedHtml {
    SanitizedHtml(sanitize_inner(html, false))
}

/// Sanitization profile for cards whose media has been ingested: also
/// admits `img`/`audio`/`video` with a `src` locked to `/media/<id>`.
pub fn sanitize_with_media(html: &str) -> SanitizedHtml {
    SanitizedHtml(sanitize_inner(html, true))
}

fn sanitize_inner(html: &str, allow_media: bool) -> String {
    let mut tags: HashSet<&str> = [
        "b", "i", "u", "em", "strong", "s", "sub", "sup", "br", "p", "div", "span", "ul", "ol",
        "li", "hr", "table", "tr", "td", "th", "code", "pre", "img",
        // Native collapsible: the hint filter and reveal-pattern translation
        // emit these; zero-JS, CSP-clean.
        "details", "summary",
    ]
    .into_iter()
    .collect();
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    // Media placeholders (chips while ingest is dark).
    tag_attributes.insert("img", ["data-media", "alt"].into_iter().collect());
    tag_attributes.insert(
        "span",
        ["data-media", "data-kind", "class"].into_iter().collect(),
    );
    tag_attributes.insert("div", ["class"].into_iter().collect());
    if allow_media {
        tags.insert("audio");
        tags.insert("video");
        tag_attributes.get_mut("img").unwrap().insert("src");
        let av: HashSet<&str> = ["src", "controls", "preload"].into_iter().collect();
        tag_attributes.insert("audio", av.clone());
        tag_attributes.insert("video", av);
    }

    ammonia::Builder::empty()
        .tags(tags)
        .tag_attributes(tag_attributes)
        .url_relative(ammonia::UrlRelative::PassThrough)
        .attribute_filter(move |element, attribute, value| {
            match attribute {
                "class" => {
                    if !matches!(element, "span" | "div") {
                        return None;
                    }
                    // Keep only tokens we emit ourselves; compound values
                    // (e.g. "cloze-answer hl-pink") keep the survivors.
                    let kept: Vec<&str> = value
                        .split_ascii_whitespace()
                        .filter(|t| ALLOWED_CLASSES.contains(t) || is_highlight_class(t))
                        .collect();
                    if kept.is_empty() {
                        None
                    } else {
                        Some(kept.join(" ").into())
                    }
                }
                // Media src must be exactly our own serving route.
                "src" => {
                    let id = value.strip_prefix("/media/")?;
                    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                        Some(value.into())
                    } else {
                        None
                    }
                }
                _ => Some(value.into()),
            }
        })
        .clean(html)
        .to_string()
}

/// Upgrades placeholder chips to real media elements for files that were
/// actually ingested (`resolve`: filename -> media id). Unresolved chips
/// stay chips. The result is re-sanitized under the media profile.
pub fn activate_media_refs(html: &str, resolve: &HashMap<String, i64>) -> SanitizedHtml {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find("<span") {
        let Some(gt_rel) = rest[lt..].find('>') else {
            break;
        };
        let body = &rest[lt + 1..lt + gt_rel];
        let close = rest[lt..].find("</span>");
        let is_chip = crate::media::attr_value(body, "class") == Some("media-ref");
        if let (true, Some(close_rel)) = (is_chip, close) {
            out.push_str(&rest[..lt]);
            let chip = &rest[lt..lt + close_rel + 7];
            let name = crate::media::attr_value(body, "data-media")
                .map(unescape_attr)
                .unwrap_or_default();
            let kind = crate::media::attr_value(body, "data-kind").unwrap_or("");
            match resolve.get(&name) {
                Some(id) => {
                    let esc = esc_attr(&name);
                    match kind {
                        "image" => out.push_str(&format!(r#"<img src="/media/{id}" alt="{esc}">"#)),
                        "audio" => out.push_str(&format!(
                            r#"<audio controls preload="none" src="/media/{id}"></audio>"#
                        )),
                        "video" => out.push_str(&format!(
                            r#"<video controls preload="none" src="/media/{id}"></video>"#
                        )),
                        _ => out.push_str(chip),
                    }
                }
                None => out.push_str(chip),
            }
            rest = &rest[lt + close_rel + 7..];
        } else {
            out.push_str(&rest[..lt + gt_rel + 1]);
            rest = &rest[lt + gt_rel + 1..];
        }
    }
    out.push_str(rest);
    sanitize_with_media(&out)
}

/// Removes the leading portion of sanitized card HTML whose *text
/// content* equals `prefix` (whitespace-insensitive), re-sanitizing the
/// remainder so cut-open tags rebalance. None when the HTML's text does
/// not start with that prefix.
pub fn strip_leading_text_html(html: &str, prefix: &str) -> Option<SanitizedHtml> {
    let want: Vec<char> = prefix.chars().filter(|c| !c.is_whitespace()).collect();
    if want.is_empty() {
        return None;
    }
    let bytes = html.as_bytes();
    let mut wi = 0usize;
    let mut i = 0usize;
    while i < bytes.len() && wi < want.len() {
        let rest = &html[i..];
        let ch = rest.chars().next().unwrap();
        if ch == '<' {
            i += rest.find('>').map(|e| e + 1)?;
            continue;
        }
        let (decoded, len) = if ch == '&' {
            // An entity is at most 8 bytes; look for its terminator within
            // that window byte-wise (';' is ASCII, so a hit is always a char
            // boundary) instead of slicing at byte 8, which may fall inside
            // a multibyte character.
            match rest.bytes().take(8).position(|b| b == b';') {
                Some(end) => {
                    let entity = &rest[..=end];
                    let decoded = match entity {
                        "&amp;" => Some('&'),
                        "&lt;" => Some('<'),
                        "&gt;" => Some('>'),
                        "&quot;" => Some('"'),
                        "&#39;" | "&apos;" => Some('\''),
                        "&nbsp;" => Some(' '),
                        _ => None,
                    };
                    match decoded {
                        // A known entity is one character for the whole of it.
                        Some(d) => (d, entity.len()),
                        // Anything else is a literal ampersand.
                        None => (ch, ch.len_utf8()),
                    }
                }
                None => (ch, ch.len_utf8()),
            }
        } else {
            (ch, ch.len_utf8())
        };
        if decoded.is_whitespace() {
            i += len;
            continue;
        }
        if decoded != want[wi] {
            return None;
        }
        wi += 1;
        i += len;
    }
    if wi < want.len() {
        return None;
    }
    Some(sanitize_card_html(&html[i..]).trimmed())
}

/// Whitespace-insensitive plain-text prefix strip: when `text` starts
/// with `prefix` (ignoring whitespace differences), returns the trimmed
/// remainder.
pub fn strip_leading_text(text: &str, prefix: &str) -> Option<String> {
    let want: Vec<char> = prefix.chars().filter(|c| !c.is_whitespace()).collect();
    if want.is_empty() {
        return None;
    }
    let mut wi = 0usize;
    let mut iter = text.char_indices();
    for (idx, ch) in iter.by_ref() {
        if ch.is_whitespace() {
            continue;
        }
        if ch != want[wi] {
            return None;
        }
        wi += 1;
        if wi == want.len() {
            let rest = &text[idx + ch.len_utf8()..];
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Removes `hl-*` highlight classes from sanitized HTML — the "clean look"
/// import choice. Spans whose class list empties keep the bare tag (the
/// sanitizer accepts attribute-less spans; they render as plain text).
/// The rewritten markup goes back through the sanitizer: the rewrite
/// reads attributes with a scanner, and only ammonia's word on what a
/// class is counts.
pub fn strip_highlight_classes(html: &str) -> SanitizedHtml {
    if !html.contains("hl-") {
        return sanitize_with_media(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find("<span") {
        let Some(gt_rel) = rest[lt..].find('>') else {
            break;
        };
        let body = &rest[lt + 1..lt + gt_rel];
        match crate::media::attr_value(body, "class") {
            Some(class) if class.contains("hl-") => {
                out.push_str(&rest[..lt]);
                let kept: Vec<&str> = class
                    .split_ascii_whitespace()
                    .filter(|t| !is_highlight_class(t))
                    .collect();
                if kept.is_empty() {
                    out.push_str("<span>");
                } else {
                    out.push_str(&format!(r#"<span class="{}">"#, kept.join(" ")));
                }
            }
            _ => out.push_str(&rest[..lt + gt_rel + 1]),
        }
        rest = &rest[lt + gt_rel + 1..];
    }
    out.push_str(rest);
    sanitize_with_media(&out)
}

fn unescape_attr(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Replaces media elements (`<img>`, `<audio>`, `<video>`, `<source>`,
/// `<object>`, `<embed>`) and `[sound:...]` tags with neutral chip spans
/// the UI styles — no broken-image jank while file ingest is dark, and a
/// stable shape to upgrade to real elements when it activates. External
/// (http/data:) references are dropped outright.
pub fn media_placeholders(html: &str) -> String {
    let html = replace_sound_tags(html);
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let tag = &rest[lt + 1..];
        let Some(end) = tag.find('>') else {
            out.push_str(&rest[lt..]);
            return out;
        };
        let body = &tag[..end];
        let name = tag_name(body);
        if matches!(
            name.as_str(),
            "img" | "audio" | "video" | "source" | "object" | "embed"
        ) {
            if !body.starts_with('/') {
                for attr in ["src", "data"] {
                    if let Some(value) = crate::media::attr_value(body, attr) {
                        if let Some(chip) = media_chip(value) {
                            out.push_str(&chip);
                        }
                        break;
                    }
                }
            }
            // The tag itself (and its closing twin) is dropped.
        } else {
            out.push('<');
            out.push_str(body);
            out.push('>');
        }
        rest = &tag[end + 1..];
    }
    out.push_str(rest);
    out
}

fn replace_sound_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("[sound:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 7..];
        match after.find(']') {
            Some(end) => {
                if let Some(chip) = media_chip(&after[..end]) {
                    out.push_str(&chip);
                }
                rest = &after[end + 1..];
            }
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn media_chip(raw: &str) -> Option<String> {
    let name = crate::media::decode_ref(raw);
    if name.is_empty() || name.contains("://") || name.starts_with("data:") {
        return None;
    }
    let kind = match classify(&name).0 {
        MediaKind::Image => "image",
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
        MediaKind::Unsupported => "file",
    };
    // The chip label is the kind, not the filename: Anki media names are
    // usually machine junk ("paste-1699...jpg"). The real name stays in
    // data-media for activation.
    let esc = esc_attr(&name);
    Some(format!(
        r#"<span class="media-ref" data-kind="{kind}" data-media="{esc}">{kind}</span>"#
    ))
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Anki's legacy LaTeX delimiters normalized to the MathJax forms KaTeX
/// renders: `[$]..[/$]` inline, `[$$]..[/$$]` display, `[latex]..[/latex]`
/// inline.
pub fn convert_latex_delims(s: &str) -> String {
    if !s.contains("[$") && !s.contains("[latex]") {
        return s.to_string();
    }
    s.replace("[$$]", "\\[")
        .replace("[/$$]", "\\]")
        .replace("[$]", "\\(")
        .replace("[/$]", "\\)")
        .replace("[latex]", "\\(")
        .replace("[/latex]", "\\)")
}

/// Removes `[anki:tts ...] ... [/anki:tts]` wrappers, keeping the inner
/// text (TTS is runtime synthesis, not content — and Flash voice mode IS
/// the TTS).
pub fn strip_tts_wrappers(s: &str) -> String {
    if !s.contains("[anki:tts") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[anki:tts") {
        out.push_str(&rest[..i]);
        match rest[i..].find(']') {
            Some(j) => rest = &rest[i + j + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.replace("[/anki:tts]", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_keeps_semantics_kills_everything_else() {
        let dirty = r#"<b>bold</b> <script>alert(1)</script><img src=x onerror=alert(1)>
            <span style="color: red" class="fancy" onclick="x()">colored</span>
            <font face="Comic Sans">ugly</font><iframe src="//evil"></iframe>
            <ul><li>one</li></ul>"#;
        let clean = sanitize_card_html(dirty);
        assert!(clean.contains("<b>bold</b>"));
        assert!(clean.contains("<li>one</li>"));
        assert!(clean.contains("<span>colored</span>"), "{clean}");
        assert!(clean.contains("ugly"), "font tag text survives");
        assert!(!clean.contains("script") && !clean.contains("alert"));
        assert!(!clean.contains("onerror") && !clean.contains("onclick"));
        assert!(!clean.contains("style") && !clean.contains("class=\"fancy\""));
        assert!(!clean.contains("iframe") && !clean.contains("font"));
    }

    #[test]
    fn sanitizer_keeps_our_own_spans_only() {
        let ours = r#"<span class="cloze-blank">[...]</span>
            <span class="media-ref" data-kind="audio" data-media="a.mp3">a.mp3</span>"#;
        let clean = sanitize_card_html(ours);
        assert!(clean.contains(r#"class="cloze-blank""#));
        assert!(clean.contains(r#"data-media="a.mp3""#));
        let foreign = sanitize_card_html(r#"<span class="evil">x</span>"#);
        assert!(!foreign.contains("class"));
    }

    #[test]
    fn highlight_classes_survive_foreign_tokens_die() {
        let mixed = r#"<span class="hl-pink evil">p</span>
            <span class="cloze-answer hl-bg-teal">t</span>
            <span class="hl-bg-mauve">no such hue</span>"#;
        let clean = sanitize_card_html(mixed);
        assert!(clean.contains(r#"class="hl-pink""#), "{clean}");
        assert!(clean.contains(r#"class="cloze-answer hl-bg-teal""#));
        assert!(!clean.contains("evil") || !clean.contains(r#"class="hl-pink evil""#));
        assert!(
            !clean.contains("mauve"),
            "unknown hue class dropped: {clean}"
        );
    }

    #[test]
    fn details_summary_survive_sanitization() {
        let html =
            r#"<details open onclick="x()"><summary class="x">Hint</summary>secret</details>"#;
        let clean = sanitize_card_html(html);
        assert!(clean.contains("<details>"), "{clean}");
        assert!(clean.contains("<summary>Hint</summary>"));
        assert!(clean.contains("secret"));
        assert!(!clean.contains("open") && !clean.contains("onclick") && !clean.contains("class"));
    }

    #[test]
    fn strip_highlights_removes_only_hl_tokens() {
        let html = r#"a <span class="hl-pink">p</span> b
            <span class="cloze-answer hl-bg-teal">t</span> <span class="cloze-blank">[x]</span>"#;
        let out = strip_highlight_classes(html);
        assert!(out.contains("<span>p</span>"), "{out}");
        assert!(out.contains(r#"<span class="cloze-answer">t</span>"#));
        assert!(out.contains(r#"class="cloze-blank""#));
        assert!(!out.contains("hl-"));
    }

    #[test]
    fn media_tags_become_chips() {
        let html = r#"before <img src="dog%20run.jpg"> mid [sound:word.mp3] after
            <video src=clip.mp4></video><img src="https://x.test/a.png">"#;
        let out = media_placeholders(html);
        assert!(
            out.contains(r#"data-kind="image" data-media="dog run.jpg""#),
            "{out}"
        );
        assert!(out.contains(r#"data-kind="audio" data-media="word.mp3""#));
        assert!(out.contains(r#"data-kind="video" data-media="clip.mp4""#));
        assert!(!out.contains("<img") && !out.contains("<video"));
        assert!(!out.contains("x.test"), "external refs dropped");
        assert!(out.contains("before") && out.contains("mid") && out.contains("after"));
    }

    #[test]
    fn activation_upgrades_resolved_chips_only() {
        let html = media_placeholders(r#"<img src="cat.jpg"> [sound:word.mp3] [sound:lost.mp3]"#);
        let clean = sanitize_card_html(&html);
        let resolve: HashMap<String, i64> =
            [("cat.jpg".to_string(), 7), ("word.mp3".to_string(), 9)].into();
        let active = activate_media_refs(&clean, &resolve);
        assert!(
            active.contains(r#"<img src="/media/7" alt="cat.jpg">"#),
            "{active}"
        );
        assert!(active.contains(r#"src="/media/9""#) && active.contains("<audio"));
        assert!(
            active.contains(r#"data-media="lost.mp3""#),
            "unresolved stays a chip"
        );
        // The media profile still refuses foreign srcs.
        let evil =
            sanitize_with_media(r#"<img src="https://evil.test/x.png"><img src="/media/3">"#);
        assert!(!evil.contains("evil.test"));
        assert!(evil.contains(r#"src="/media/3""#));
    }

    #[test]
    fn leading_text_strips_across_markup() {
        assert_eq!(
            strip_leading_text("Q text\nanswer", "Q text").as_deref(),
            Some("answer")
        );
        assert_eq!(strip_leading_text("different", "Q"), None);
        let html = sanitize_card_html("<div><b>Q</b> text</div><div>answer <i>rest</i></div>");
        let cut = strip_leading_text_html(&html, "Q text").unwrap();
        assert!(
            cut.contains("answer") && cut.contains("<i>rest</i>"),
            "{cut}"
        );
        assert!(!cut.contains('Q'));
        assert!(strip_leading_text_html(&html, "unrelated").is_none());
    }

    #[test]
    fn latex_delimiters_convert() {
        assert_eq!(
            convert_latex_delims("a [$]x^2[/$] b [$$]\\sum[/$$] c [latex]\\frac12[/latex]"),
            "a \\(x^2\\) b \\[\\sum\\] c \\(\\frac12\\)"
        );
        assert_eq!(convert_latex_delims("plain"), "plain");
    }

    #[test]
    fn tts_wrappers_strip_to_inner_text() {
        assert_eq!(
            strip_tts_wrappers("[anki:tts lang=ja_JP]こんにちは[/anki:tts] world"),
            "こんにちは world"
        );
    }
}

#[cfg(test)]
mod media_id_tests {
    use super::media_image_ids;

    #[test]
    fn images_only_deduped_in_order() {
        let html = r#"<p><img src="/media/7" alt="a"> text <audio controls preload="none" src="/media/8"></audio><img src="/media/9"><img src="/media/7"></p>"#;
        assert_eq!(media_image_ids(html), vec![7, 9]);
        assert!(media_image_ids("<b>none</b>").is_empty());
    }
}
