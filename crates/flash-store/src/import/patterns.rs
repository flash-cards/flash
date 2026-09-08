//! Recognizes the dominant card-template JavaScript idiom — a clickable
//! element that reveals a hidden target (AnKing-style hint buttons,
//! tap-to-reveal sections) — and translates it to a native `<details>`
//! collapsible at import. Foreign JavaScript is NEVER executed: anything
//! this module doesn't confidently recognize is left for the sanitizer to
//! strip exactly as before. Non-goals: timers, typing games, per-card
//! state, anything beyond show/hide.

use crate::media::tag_name;

/// Rewrites `trigger[onclick -> #id]` + hidden `#id` element pairs into
/// `<details><summary>label</summary>content</details>`. Conservative:
/// requires an explicit id reference and a hidden target in the same
/// fragment; both orders (trigger before/after target) are handled.
pub fn translate_reveal_patterns(html: &str) -> String {
    if !html.contains("onclick") {
        return html.to_string();
    }
    let mut out = html.to_string();
    // Bounded: each iteration removes one trigger's onclick pair.
    for _ in 0..20 {
        match rewrite_one(&out) {
            Some(next) => out = next,
            None => break,
        }
    }
    out
}

fn rewrite_one(html: &str) -> Option<String> {
    let mut search = 0usize;
    loop {
        let (tag_start, body) = next_tag(html, search)?;
        let body_end = tag_start + 1 + body.len();
        search = body_end + 1;
        if body.starts_with('/') {
            continue;
        }
        let Some(onclick) = crate::media::attr_value(body, "onclick") else {
            continue;
        };
        let Some(target_id) = referenced_id(onclick) else {
            continue;
        };
        // The trigger element's full span (open tag .. matching close).
        let trigger_name = tag_name(body);
        let Some(trigger_end) = element_end(html, tag_start, &trigger_name) else {
            continue;
        };
        let label = strip_tags(&html[body_end + 1..trigger_end.0]);
        let label = label.trim();
        // Find the hidden target anywhere in the fragment.
        let Some((t_start, t_body_end, t_close_start, t_close_end)) =
            find_element_by_id(html, &target_id)
        else {
            continue;
        };
        let t_body = &html[t_start + 1..t_body_end];
        if !is_hidden(t_body) {
            continue;
        }
        let content = &html[t_body_end + 1..t_close_start];
        let label = if label.is_empty() { "Show more" } else { label };
        let replacement = format!("<details><summary>{label}</summary>{content}</details>");
        // Remove target and replace trigger, handling either order.
        let (a, b) = if t_start < tag_start {
            // target first: cut target span, then trigger span.
            (
                (t_start, t_close_end, String::new()),
                (tag_start, trigger_end.1, replacement),
            )
        } else if t_start >= trigger_end.1 {
            (
                (tag_start, trigger_end.1, replacement),
                (t_start, t_close_end, String::new()),
            )
        } else {
            // Target nested inside trigger: unsupported shape.
            continue;
        };
        let mut out = String::with_capacity(html.len());
        out.push_str(&html[..a.0]);
        out.push_str(&a.2);
        out.push_str(&html[a.1..b.0]);
        out.push_str(&b.2);
        out.push_str(&html[b.1..]);
        return Some(out);
    }
}

/// `document.getElementById('X')` / `querySelector('#X')` -> X.
fn referenced_id(onclick: &str) -> Option<String> {
    for (marker, hash) in [("getElementById(", false), ("querySelector(", true)] {
        if let Some(i) = onclick.find(marker) {
            let after = &onclick[i + marker.len()..];
            let quote = after.chars().next()?;
            if quote != '"' && quote != '\'' {
                continue;
            }
            let inner = &after[1..after[1..].find(quote)? + 1];
            let id = if hash {
                inner.strip_prefix('#')?
            } else {
                inner
            };
            if !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn is_hidden(tag_body: &str) -> bool {
    if let Some(style) = crate::media::attr_value(tag_body, "style") {
        let s: String = style.chars().filter(|c| !c.is_whitespace()).collect();
        if s.to_ascii_lowercase().contains("display:none") {
            return true;
        }
    }
    if tag_body
        .split_ascii_whitespace()
        .any(|t| t == "hidden" || t == "hidden=\"\"")
    {
        return true;
    }
    crate::media::attr_value(tag_body, "class")
        .map(|c| c.split_ascii_whitespace().any(|t| t == "hidden"))
        .unwrap_or(false)
}

/// (start-of-`<` offset, tag body) of the next tag at/after `from`.
fn next_tag(html: &str, from: usize) -> Option<(usize, &str)> {
    let lt = html[from..].find('<')? + from;
    let gt = html[lt..].find('>')? + lt;
    Some((lt, &html[lt + 1..gt]))
}

/// End of the element opened at `open_lt`: ((inner-end offset), (offset
/// just past the closing tag)). Tracks nesting of the same tag name.
fn element_end(html: &str, open_lt: usize, name: &str) -> Option<(usize, usize)> {
    if matches!(name, "br" | "hr" | "img" | "input") {
        return None;
    }
    let mut depth = 1i32;
    let mut pos = html[open_lt..].find('>')? + open_lt + 1;
    loop {
        let (lt, body) = next_tag(html, pos)?;
        let gt = lt + 1 + body.len();
        let n = tag_name(body);
        if n == name {
            if body.starts_with('/') {
                depth -= 1;
                if depth == 0 {
                    return Some((lt, gt + 1));
                }
            } else if !body.ends_with('/') {
                depth += 1;
            }
        }
        pos = gt + 1;
    }
}

/// Full span of the element carrying `id`: (open `<` offset, open-tag end
/// offset, closing-tag `<` offset, offset past `</name>`).
fn find_element_by_id(html: &str, id: &str) -> Option<(usize, usize, usize, usize)> {
    let mut pos = 0usize;
    loop {
        let (lt, body) = next_tag(html, pos)?;
        let gt = lt + 1 + body.len();
        pos = gt + 1;
        if body.starts_with('/') {
            continue;
        }
        if crate::media::attr_value(body, "id") == Some(id) {
            let name = tag_name(body);
            let (close_lt, close_end) = element_end(html, lt, &name)?;
            return Some((lt, gt, close_lt, close_end));
        }
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match (in_tag, ch) {
            (false, '<') => in_tag = true,
            (true, '>') => in_tag = false,
            (false, c) => out.push(c),
            (true, _) => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anking_style_hint_button_becomes_details() {
        let html = r##"before <a class="hint" href="#"
            onclick="this.style.display='none';document.getElementById('hint42').style.display='';return false;">
            Show <b>Mnemonic</b></a>
            <div id="hint42" class="hint" style="display: none">The <i>trick</i> is X</div> after"##;
        let out = translate_reveal_patterns(html);
        assert!(
            out.contains(
                "<details><summary>Show Mnemonic</summary>The <i>trick</i> is X</details>"
            ),
            "{out}"
        );
        assert!(!out.contains("onclick") || !out.contains("hint42"), "{out}");
        assert!(out.contains("before") && out.contains("after"));
    }

    #[test]
    fn target_before_trigger_and_query_selector() {
        let html = r#"<div id="extra" style="display:none"><ul><li>a</li></ul></div>
            <button onclick="document.querySelector('#extra').style.display='block'">Reveal extra</button>"#;
        let out = translate_reveal_patterns(html);
        assert!(
            out.contains("<details><summary>Reveal extra</summary><ul><li>a</li></ul></details>"),
            "{out}"
        );
    }

    #[test]
    fn unrecognized_shapes_pass_through() {
        // Visible target: not a reveal pattern.
        let visible = r#"<a onclick="document.getElementById('x').scrollIntoView()">go</a><div id="x">v</div>"#;
        assert_eq!(translate_reveal_patterns(visible), visible);
        // No id reference at all.
        let timer = r#"<div onclick="startTimer()">tick</div>"#;
        assert_eq!(translate_reveal_patterns(timer), timer);
        // No onclick: untouched fast path.
        let plain = "<b>hello</b>";
        assert_eq!(translate_reveal_patterns(plain), plain);
    }

    #[test]
    fn pathological_inputs_terminate_untransformed() {
        // Hundreds of triggers with missing targets: nothing to transform.
        let many: String = (0..300)
            .map(|i| format!(r#"<a onclick="document.getElementById('nope{i}').x()">t{i}</a>"#))
            .collect();
        let out = translate_reveal_patterns(&many);
        assert_eq!(out, many);
        // Unterminated tag, self-reference, target nested inside trigger.
        for nasty in [
            "<a onclick=\"document.getElementById('x National",
            r#"<div id="x" onclick="document.getElementById('x').style.display=''">self</div>"#,
            r#"<div onclick="document.getElementById('in').style.display=''">t<span id="in" style="display:none">n</span></div>"#,
            r#"<a onclick="getElementById(">broken</a>"#,
        ] {
            let out = translate_reveal_patterns(nasty);
            assert_eq!(out, nasty, "left untouched");
        }
        // Deep same-name nesting still resolves the element span.
        let deep = format!(
            r#"<a onclick="document.getElementById('d').style.display=''">go</a><div id="d" style="display:none">{}x{}</div>"#,
            "<div>".repeat(80),
            "</div>".repeat(80)
        );
        let out = translate_reveal_patterns(&deep);
        assert!(out.contains("<details><summary>go</summary>"), "{out}");
    }

    #[test]
    fn multiple_pairs_all_translate() {
        let html = r#"<a onclick="document.getElementById('h1').style.display=''">One</a>
            <div id="h1" style="display:none">first</div>
            <a onclick="document.getElementById('h2').style.display=''">Two</a>
            <div id="h2" style="display:none">second</div>"#;
        let out = translate_reveal_patterns(html);
        assert!(out.contains("<summary>One</summary>first"), "{out}");
        assert!(out.contains("<summary>Two</summary>second"), "{out}");
        assert!(!out.contains("onclick"), "{out}");
    }
}
