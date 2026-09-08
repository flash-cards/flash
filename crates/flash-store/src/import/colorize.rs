//! Import-time color remapping: imported color *intent* survives, but every
//! color is translated into Flash's palette. Inline `style` colors,
//! `<font color>`, `<mark>`, and (best-effort) notetype-CSS class colors
//! become `hl-{hue}` (text) / `hl-bg-{hue}` (highlight) classes that the
//! design system styles per theme. Near-neutral colors (low saturation,
//! near-black/white) are treated as unstyled — they were contrast tweaks,
//! not meaning. Coverage is deliberately span/div/font/mark: that is what
//! Anki's editor emits; styles on other tags stay stripped.
//!
//! Runs before sanitization; the sanitizer admits only validated `hl-*`
//! tokens, so nothing here widens the security surface.

use std::collections::HashMap;

use crate::media::tag_name;

/// Anki class name -> emitted highlight class (e.g. "hl-bg-pink").
pub type ClassColorMap = HashMap<String, String>;

/// Extracts simple color rules from a notetype's CSS blob: single-class
/// selectors (`.name { … }`) only; `background(-color)` wins over `color`
/// (highlight intent). Anything fancier is ignored.
pub fn parse_css_class_colors(css: &str) -> ClassColorMap {
    let mut map = ClassColorMap::new();
    let mut rest = css;
    while let Some(open) = rest.find('{') {
        let selector = rest[..open]
            .rsplit(&['}', ';'][..])
            .next()
            .unwrap_or("")
            .trim();
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let block = &rest[open + 1..open + close];
        if let Some(name) = single_class_selector(selector) {
            let bg =
                decl_value(block, "background-color").or_else(|| decl_value(block, "background"));
            let mapped = match bg.and_then(parse_color).and_then(bucket) {
                Some(hue) => Some(format!("hl-bg-{hue}")),
                None => decl_value(block, "color")
                    .and_then(parse_color)
                    .and_then(bucket)
                    .map(|hue| format!("hl-{hue}")),
            };
            if let Some(class) = mapped {
                map.insert(name.to_string(), class);
            }
        }
        rest = &rest[open + close + 1..];
    }
    map
}

fn single_class_selector(sel: &str) -> Option<&str> {
    let name = sel.strip_prefix('.')?;
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(name)
    } else {
        None
    }
}

/// First value of `prop:` in a CSS declaration block.
fn decl_value<'a>(block: &'a str, prop: &str) -> Option<&'a str> {
    let mut rest = block;
    loop {
        let i = find_ascii_ci(rest, prop)?;
        let after = &rest[i + prop.len()..];
        // Must be a property name boundary, not a suffix of another prop.
        let before_ok = rest[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-');
        let after_colon = after.trim_start();
        if before_ok && after_colon.starts_with(':') {
            let value = after_colon[1..].split(&[';', '}'][..]).next()?.trim();
            return Some(value);
        }
        rest = &rest[i + prop.len()..];
    }
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Rewrites raw card HTML: styled spans/divs gain `hl-*` classes,
/// `<font color>` and `<mark>` become classed spans. Pure; unmatched
/// content passes through untouched.
pub fn remap_colors(html: &str, class_map: &ClassColorMap) -> String {
    let quick = html.contains("style=")
        || html.contains("<font")
        || html.contains("<mark")
        || (!class_map.is_empty() && html.contains("class="));
    if !quick {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let tag = &rest[lt + 1..];
        let Some(end) = tag.find('>') else {
            out.push_str(&rest[lt..]);
            return out;
        };
        let body = &tag[..end];
        let name = tag_name(body);
        let closing = body.starts_with('/');
        match (closing, name.as_str()) {
            (true, "font" | "mark") => out.push_str("</span>"),
            (false, "font") => {
                let class = crate::media::attr_value(body, "color")
                    .and_then(parse_color)
                    .and_then(bucket)
                    .map(|hue| format!("hl-{hue}"));
                match class {
                    Some(c) => out.push_str(&format!(r#"<span class="{c}">"#)),
                    None => out.push_str("<span>"),
                }
            }
            (false, "mark") => out.push_str(r#"<span class="hl-bg-amber">"#),
            (false, "span" | "div") => {
                let mut extra: Vec<String> = Vec::new();
                if let Some(style) = crate::media::attr_value(body, "style") {
                    let bg = decl_value(style, "background-color")
                        .or_else(|| decl_value(style, "background"))
                        .and_then(parse_color)
                        .and_then(bucket);
                    if let Some(hue) = bg {
                        extra.push(format!("hl-bg-{hue}"));
                    }
                    if let Some(hue) = decl_value(style, "color")
                        .and_then(parse_color)
                        .and_then(bucket)
                    {
                        extra.push(format!("hl-{hue}"));
                    }
                }
                let existing = crate::media::attr_value(body, "class").unwrap_or("");
                for token in existing.split_ascii_whitespace() {
                    if let Some(mapped) = class_map.get(token) {
                        if !extra.contains(mapped) {
                            extra.push(mapped.clone());
                        }
                    }
                }
                if extra.is_empty() {
                    out.push('<');
                    out.push_str(body);
                    out.push('>');
                } else {
                    let mut classes: Vec<&str> = existing.split_ascii_whitespace().collect();
                    for e in &extra {
                        if !classes.contains(&e.as_str()) {
                            classes.push(e);
                        }
                    }
                    let stripped = remove_attr(body, "class");
                    out.push('<');
                    out.push_str(stripped.trim_end());
                    out.push_str(&format!(r#" class="{}">"#, classes.join(" ")));
                }
            }
            _ => {
                out.push('<');
                out.push_str(body);
                out.push('>');
            }
        }
        rest = &tag[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Removes a quoted attribute from a tag body (best effort; leaves the
/// body unchanged when the attribute isn't found in quoted form).
fn remove_attr(body: &str, attr: &str) -> String {
    let pattern = format!("{attr}=");
    let Some(i) = find_ascii_ci(body, &pattern) else {
        return body.to_string();
    };
    let boundary_ok = body[..i]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_ascii_whitespace());
    let after = &body[i + pattern.len()..];
    let quote = after.chars().next();
    if !boundary_ok || !matches!(quote, Some('"') | Some('\'')) {
        return body.to_string();
    }
    let q = quote.unwrap();
    let Some(close) = after[1..].find(q) else {
        return body.to_string();
    };
    let mut out = String::with_capacity(body.len());
    out.push_str(body[..i].trim_end());
    out.push(' ');
    out.push_str(after[1 + close + 1..].trim_start());
    out.trim_end().to_string()
}

// ---- color parsing ----

/// CSS color -> (hue degrees, saturation 0-1, lightness 0-1).
fn parse_color(value: &str) -> Option<(f32, f32, f32)> {
    let v = value
        .trim()
        .trim_matches(&['"', '\''][..])
        .trim()
        .to_ascii_lowercase();
    let (r, g, b) = if let Some(hex) = v.strip_prefix('#') {
        // Non-ASCII can never be a hex colour, and byte-slicing it below
        // would panic on a char boundary.
        if !hex.is_ascii() {
            return None;
        }
        match hex.len() {
            3 => {
                let n = u32::from_str_radix(hex, 16).ok()?;
                (
                    (((n >> 8) & 0xf) * 17) as f32,
                    (((n >> 4) & 0xf) * 17) as f32,
                    ((n & 0xf) * 17) as f32,
                )
            }
            6 | 8 => {
                let n = u32::from_str_radix(&hex[..6], 16).ok()?;
                (
                    ((n >> 16) & 255) as f32,
                    ((n >> 8) & 255) as f32,
                    (n & 255) as f32,
                )
            }
            _ => return None,
        }
    } else if v.starts_with("rgb") {
        let inner = v
            .find('(')
            .and_then(|o| v[o + 1..].find(')').map(|c| &v[o + 1..o + 1 + c]))?;
        let mut parts = inner.split(&[',', ' ', '/'][..]).filter(|s| !s.is_empty());
        let chan = |p: &str| -> Option<f32> {
            if let Some(pc) = p.strip_suffix('%') {
                Some(pc.trim().parse::<f32>().ok()? * 2.55)
            } else {
                p.trim().parse::<f32>().ok()
            }
        };
        let r = chan(parts.next()?)?;
        let g = chan(parts.next()?)?;
        let b = chan(parts.next()?)?;
        (r, g, b)
    } else {
        named_color(&v)?
    };
    Some(rgb_to_hsl(r / 255.0, g / 255.0, b / 255.0))
}

fn named_color(name: &str) -> Option<(f32, f32, f32)> {
    let (r, g, b): (u8, u8, u8) = match name {
        "red" => (255, 0, 0),
        "darkred" | "maroon" => (139, 0, 0),
        "crimson" => (220, 20, 60),
        "tomato" => (255, 99, 71),
        "coral" => (255, 127, 80),
        "salmon" => (250, 128, 114),
        "orange" => (255, 165, 0),
        "darkorange" => (255, 140, 0),
        "gold" => (255, 215, 0),
        "yellow" => (255, 255, 0),
        "khaki" => (240, 230, 140),
        "green" => (0, 128, 0),
        "darkgreen" => (0, 100, 0),
        "lime" => (0, 255, 0),
        "limegreen" => (50, 205, 50),
        "seagreen" => (46, 139, 87),
        "olive" => (128, 128, 0),
        "teal" => (0, 128, 128),
        "cyan" | "aqua" => (0, 255, 255),
        "turquoise" => (64, 224, 208),
        "blue" => (0, 0, 255),
        "navy" => (0, 0, 128),
        "royalblue" => (65, 105, 225),
        "dodgerblue" => (30, 144, 255),
        "skyblue" => (135, 206, 235),
        "lightblue" => (173, 216, 230),
        "purple" => (128, 0, 128),
        "indigo" => (75, 0, 130),
        "violet" => (238, 130, 238),
        "magenta" | "fuchsia" => (255, 0, 255),
        "orchid" => (218, 112, 214),
        "pink" => (255, 192, 203),
        "hotpink" => (255, 105, 180),
        "deeppink" => (255, 20, 147),
        "brown" => (165, 42, 42),
        _ => return None,
    };
    Some((r as f32, g as f32, b as f32))
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s, l)
}

/// Hue bucket, or None for near-neutral colors (contrast tweaks, not
/// meaning). Buckets match `richtext::HIGHLIGHT_HUES`.
fn bucket(hsl: (f32, f32, f32)) -> Option<&'static str> {
    let (h, s, l) = hsl;
    if s < 0.15 || !(0.08..=0.95).contains(&l) {
        return None;
    }
    Some(match h {
        h if h < 15.0 => "red",
        h if h < 45.0 => "amber",
        h if h < 70.0 => "amber", // yellow reads as amber in the palette
        h if h < 170.0 => "green",
        h if h < 200.0 => "teal",
        h if h < 255.0 => "blue",
        h if h < 305.0 => "purple", // includes CSS purple/magenta at 300
        h if h < 345.0 => "pink",
        _ => "red",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_bucket_and_neutrals_drop() {
        let hue = |v: &str| parse_color(v).and_then(bucket);
        assert_eq!(hue("#ff0000"), Some("red"));
        assert_eq!(hue("rgb(255, 105, 180)"), Some("pink"));
        assert_eq!(hue("hotpink"), Some("pink"));
        assert_eq!(hue("#ff0"), Some("amber"));
        assert_eq!(hue("dodgerblue"), Some("blue"));
        assert_eq!(hue("purple"), Some("purple"));
        assert_eq!(hue("teal"), Some("teal"));
        assert_eq!(hue("#808080"), None, "gray is neutral");
        assert_eq!(hue("#0a0a0b"), None, "near-black is neutral");
        assert_eq!(hue("white"), None, "unknown/neutral name");
    }

    #[test]
    fn styled_spans_gain_classes_font_and_mark_convert() {
        let map = ClassColorMap::new();
        let html = r#"a <span style="color: #ff69b4">p</span>
            <span style="background-color: yellow;">y</span>
            <font color="red">r</font> <mark>m</mark> <b>plain</b>"#;
        let out = remap_colors(html, &map);
        assert!(
            out.contains(r#"<span style="color: #ff69b4" class="hl-pink">p</span>"#),
            "{out}"
        );
        assert!(out.contains(r#"class="hl-bg-amber">y</span>"#));
        assert!(out.contains(r#"<span class="hl-red">r</span>"#));
        assert!(out.contains(r#"<span class="hl-bg-amber">m</span>"#));
        assert!(out.contains("<b>plain</b>"));
        assert!(!out.contains("<font") && !out.contains("<mark"));
    }

    #[test]
    fn notetype_css_classes_resolve() {
        let map = parse_css_class_colors(
            ".hint { color: #2f6cb3; font-weight: bold }\n.mark1{background: hotpink}\n.card, .other { color: red }",
        );
        assert_eq!(map.get("hint").map(String::as_str), Some("hl-blue"));
        assert_eq!(map.get("mark1").map(String::as_str), Some("hl-bg-pink"));
        assert!(!map.contains_key("card"), "multi-selector rules ignored");
        let html = r#"<span class="mark1">x</span>"#;
        let out = remap_colors(html, &map);
        assert!(out.contains(r#"class="mark1 hl-bg-pink""#), "{out}");
    }

    #[test]
    fn pathological_inputs_terminate() {
        let map = ClassColorMap::new();
        // 10k styled spans; giant junk style values; broken markup.
        let many: String = (0..10_000)
            .map(|i| format!(r#"<span style="color: #ff000{}">x</span>"#, i % 10))
            .collect();
        let out = remap_colors(&many, &map);
        assert!(out.contains("hl-red"));
        for nasty in [
            "<span style=\"color: rgb(999,,,)\">x</span>",
            "<span style=\"color:\">x</span>",
            "<font color=>y</font>",
            "<span style='unterminated",
            &format!("<span style=\"color: {}\">x</span>", "#".repeat(50_000)),
        ] {
            let _ = remap_colors(nasty, &map); // must not panic
        }
        // CSS parser survives junk blocks.
        let _ =
            parse_css_class_colors("} } { .a { color } .b { { } @media x { .c { color: red } }");
    }

    #[test]
    fn existing_class_attr_is_merged_not_duplicated() {
        let map = ClassColorMap::new();
        let html = r#"<span class="keep" style="color:red">x</span>"#;
        let out = remap_colors(html, &map);
        assert!(out.contains(r#"class="keep hl-red""#), "{out}");
        assert_eq!(out.matches("class=").count(), 1, "{out}");
    }
}
