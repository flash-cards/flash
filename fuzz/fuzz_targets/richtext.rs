//! Card HTML on every path it travels: the sanitizer (with and without
//! media), plain-text extraction, media id scanning and remapping,
//! placeholder rewriting, the leading-prefix strippers, highlight and
//! TTS cleanup, LaTeX delimiter conversion, and the CSS colour remap the
//! Anki importer applies. Each must return on any string.

#![no_main]

use std::collections::HashMap;

use flash_store::import::{parse_css_class_colors, remap_colors};
use flash_store::richtext::{
    activate_media_refs, convert_latex_delims, html_to_text, media_ids, media_image_ids,
    media_placeholders, remap_media_ids, sanitize_card_html, sanitize_with_media,
    strip_highlight_classes, strip_leading_text, strip_leading_text_html, strip_tts_wrappers,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // The first line doubles as a prefix / CSS blob; the rest is the HTML.
    let (head, html) = s.split_once('\n').unwrap_or(("", s));

    let clean = sanitize_card_html(html);
    let _ = sanitize_with_media(html);
    let _ = html_to_text(html);
    let _ = html_to_text(&clean);
    let ids = media_ids(html);
    let _ = media_image_ids(html);
    let map: HashMap<i64, i64> = ids.iter().map(|id| (*id, id.wrapping_add(1))).collect();
    let _ = remap_media_ids(html, &map);
    let _ = media_placeholders(html);
    let resolve: HashMap<String, i64> = [(head.to_string(), 1i64)].into_iter().collect();
    let _ = activate_media_refs(html, &resolve);
    let _ = strip_leading_text_html(html, head);
    let _ = strip_leading_text(html, head);
    let _ = strip_highlight_classes(html);
    let _ = strip_tts_wrappers(html);
    let _ = convert_latex_delims(html);
    let colors = parse_css_class_colors(head);
    let _ = remap_colors(html, &colors);
});
