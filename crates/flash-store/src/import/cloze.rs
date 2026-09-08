//! Anki cloze deletion parsing: `{{cN::answer}}` / `{{cN::answer::hint}}`,
//! nesting-aware (`{{c2::A {{c3::B}}}}` is legal Anki). One Flash card is
//! generated per distinct index; rendering happens on the Seg tree so a
//! blanked span hides its whole content while other indices resolve to
//! their answers. Operates on raw field HTML — brace matching is
//! HTML-agnostic and stripping/sanitizing happens downstream.

use std::collections::BTreeSet;

/// Bounds card explosion from hostile/megalomaniac notes (Anki itself
/// allows ~499 indices; real decks use a handful).
pub const MAX_CLOZE_INDICES: usize = 50;

#[derive(Debug, Clone)]
enum Seg {
    Text(String),
    Cloze {
        index: u32,
        answer: Vec<Seg>,
        hint: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ClozeNote {
    segs: Vec<Seg>,
    indices: BTreeSet<u32>,
}

/// None when the text contains no valid cloze span (malformed markup
/// stays literal text and the note imports as a standard card).
/// How deep `{{cN::{{cM::…}}}}` may nest before an inner `{{c` is left as
/// literal text. Anki itself nests one or two levels; the cap exists so a
/// hostile field cannot drive the recursive parser (and the renderers
/// that mirror its shape) off the stack.
const MAX_NESTING: usize = 8;

pub fn parse(text: &str) -> Option<ClozeNote> {
    let mut indices = BTreeSet::new();
    let segs = parse_segs(text, &mut indices);
    if indices.is_empty() {
        None
    } else {
        Some(ClozeNote { segs, indices })
    }
}

impl ClozeNote {
    /// Distinct indices, ascending, capped.
    pub fn indices(&self) -> Vec<u32> {
        self.indices
            .iter()
            .copied()
            .take(MAX_CLOZE_INDICES)
            .collect()
    }

    /// The text with index-`target` spans blanked (`[...]` or `[hint]`,
    /// wrapped in our styled span when `html`) and every other index
    /// resolved to its answer.
    pub fn front(&self, target: u32, html: bool) -> String {
        render_segs(&self.segs, Some(target), html)
    }

    /// Every index-`target` answer, in order (text form).
    pub fn answers(&self, target: u32) -> Vec<String> {
        let mut out = Vec::new();
        collect_answers(&self.segs, target, &mut out);
        out
    }

    /// The completed sentence: every index resolved, with index-`target`
    /// answers wrapped in a highlight span — what the reveal shows.
    pub fn filled(&self, target: u32) -> String {
        render_filled(&self.segs, target)
    }
}

fn render_filled(segs: &[Seg], target: u32) -> String {
    let mut out = String::new();
    for seg in segs {
        match seg {
            Seg::Text(t) => out.push_str(t),
            Seg::Cloze { index, answer, .. } => {
                let inner = render_segs(answer, None, false);
                if *index == target {
                    // An inline span cannot legally contain line breaks or
                    // block elements — the HTML parser would split the
                    // highlight apart. Multi-line answers get a block
                    // highlight instead.
                    let lower = inner.to_ascii_lowercase();
                    let block = ["<br", "<div", "<p", "<ul", "<ol", "<li", "<table"]
                        .iter()
                        .any(|t| lower.contains(t));
                    if block {
                        out.push_str(r#"<div class="cloze-answer-block">"#);
                        out.push_str(&inner);
                        out.push_str("</div>");
                    } else {
                        out.push_str(r#"<span class="cloze-answer">"#);
                        out.push_str(&inner);
                        out.push_str("</span>");
                    }
                } else {
                    out.push_str(&inner);
                }
            }
        }
    }
    out
}

fn render_segs(segs: &[Seg], target: Option<u32>, html: bool) -> String {
    let mut out = String::new();
    for seg in segs {
        match seg {
            Seg::Text(t) => out.push_str(t),
            Seg::Cloze {
                index,
                answer,
                hint,
            } => {
                if Some(*index) == target {
                    let blank = match hint {
                        Some(h) => format!("[{h}]"),
                        None => "[...]".to_string(),
                    };
                    if html {
                        out.push_str(r#"<span class="cloze-blank">"#);
                        out.push_str(&blank);
                        out.push_str("</span>");
                    } else {
                        out.push_str(&blank);
                    }
                } else {
                    out.push_str(&render_segs(answer, target, html));
                }
            }
        }
    }
    out
}

fn collect_answers(segs: &[Seg], target: u32, out: &mut Vec<String>) {
    for seg in segs {
        if let Seg::Cloze { index, answer, .. } = seg {
            if *index == target {
                out.push(render_segs(answer, None, false));
            } else {
                collect_answers(answer, target, out);
            }
        }
    }
}

/// Segments under construction: the finished ones plus the text run
/// that follows them.
#[derive(Default)]
struct Buf {
    segs: Vec<Seg>,
    text: String,
}

impl Buf {
    fn push_text(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn push_seg(&mut self, seg: Seg) {
        if !self.text.is_empty() {
            self.segs.push(Seg::Text(std::mem::take(&mut self.text)));
        }
        self.segs.push(seg);
    }

    /// Appends another buffer's content in order.
    fn extend(&mut self, other: Buf) {
        for seg in other.segs {
            match seg {
                Seg::Text(t) => self.text.push_str(&t),
                cloze => self.push_seg(cloze),
            }
        }
        self.text.push_str(&other.text);
    }

    fn finish(mut self) -> Vec<Seg> {
        if !self.text.is_empty() {
            self.segs.push(Seg::Text(self.text));
        }
        self.segs
    }
}

/// An open `{{` awaiting its `}}`.
enum Frame {
    /// A cloze span: its header (`{{c1::`) kept for the case it never
    /// closes, the answer so far, and once the top-level `::` has been
    /// seen, the raw hint with the brace depth inside it.
    Cloze {
        index: u32,
        header: String,
        answer: Buf,
        hint: Option<(String, usize)>,
    },
    /// A plain `{{ … }}` inside a cloze (template syntax, or a cloze
    /// opener past the nesting cap): balanced for the enclosing span,
    /// literal text in the result.
    Brace(Buf),
}

/// One pass over the field with a stack of open braces, so the cost is
/// linear in the field's length however many openers never close. The
/// recursive form rescanned to the end of the field for each unterminated
/// `{{c`, which made a 64 KB field of them cost seconds.
fn parse_segs(s: &str, indices: &mut BTreeSet<u32>) -> Vec<Seg> {
    let mut root = Buf::default();
    let mut stack: Vec<Frame> = Vec::new();
    let mut cloze_depth = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        // Inside a hint, only braces matter and they are kept as text.
        if let Some(Frame::Cloze {
            hint: Some((raw, depth)),
            ..
        }) = stack.last_mut()
        {
            if rest.starts_with("{{") {
                *depth += 1;
                raw.push_str("{{");
                i += 2;
                continue;
            }
            if rest.starts_with("}}") {
                if *depth > 0 {
                    *depth -= 1;
                    raw.push_str("}}");
                    i += 2;
                    continue;
                }
                let frame = stack.pop().expect("frame just seen");
                cloze_depth -= 1;
                let seg = close_cloze(frame, indices);
                top(&mut stack, &mut root).push_seg(seg);
                i += 2;
                continue;
            }
            let ch = rest.chars().next().expect("non-empty rest");
            raw.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if rest.starts_with("{{") {
            if cloze_depth < MAX_NESTING {
                if let Some((index, header_len)) = cloze_header(rest) {
                    stack.push(Frame::Cloze {
                        index,
                        header: rest[..header_len].to_string(),
                        answer: Buf::default(),
                        hint: None,
                    });
                    cloze_depth += 1;
                    i += header_len;
                    continue;
                }
            }
            if stack.is_empty() {
                // Outside any span a plain `{{` is just text.
                root.push_text("{{");
            } else {
                stack.push(Frame::Brace(Buf::default()));
            }
            i += 2;
            continue;
        }
        if rest.starts_with("}}") {
            match stack.pop() {
                None => root.push_text("}}"),
                Some(Frame::Brace(buf)) => {
                    let parent = top(&mut stack, &mut root);
                    parent.push_text("{{");
                    parent.extend(buf);
                    parent.push_text("}}");
                }
                Some(frame) => {
                    cloze_depth -= 1;
                    let seg = close_cloze(frame, indices);
                    top(&mut stack, &mut root).push_seg(seg);
                }
            }
            i += 2;
            continue;
        }
        if rest.starts_with("::") {
            if let Some(Frame::Cloze { hint, .. }) = stack.last_mut() {
                if hint.is_none() {
                    *hint = Some((String::new(), 0));
                    i += 2;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().expect("non-empty rest");
        let mut tmp = [0u8; 4];
        top(&mut stack, &mut root).push_text(ch.encode_utf8(&mut tmp));
        i += ch.len_utf8();
    }

    // Whatever never closed is literal text, its content kept (a complete
    // cloze inside an unterminated one still counts).
    while let Some(frame) = stack.pop() {
        let parent = top(&mut stack, &mut root);
        match frame {
            Frame::Brace(buf) => {
                parent.push_text("{{");
                parent.extend(buf);
            }
            Frame::Cloze {
                header,
                answer,
                hint,
                ..
            } => {
                parent.push_text(&header);
                parent.extend(answer);
                if let Some((raw, _)) = hint {
                    parent.push_text("::");
                    parent.push_text(&raw);
                }
            }
        }
    }
    root.finish()
}

/// The buffer new content goes into: the innermost open frame's, or the
/// field's own.
fn top<'a>(stack: &'a mut [Frame], root: &'a mut Buf) -> &'a mut Buf {
    match stack.last_mut() {
        Some(Frame::Cloze { answer, .. }) => answer,
        Some(Frame::Brace(buf)) => buf,
        None => root,
    }
}

fn close_cloze(frame: Frame, indices: &mut BTreeSet<u32>) -> Seg {
    let Frame::Cloze {
        index,
        answer,
        hint,
        ..
    } = frame
    else {
        unreachable!("only cloze frames are closed here")
    };
    indices.insert(index);
    Seg::Cloze {
        index,
        answer: answer.finish(),
        hint: hint.map(|(raw, _)| raw),
    }
}

/// `{{cN::` at the start of `s`: the index and the header's length, or
/// None when what follows `{{c` is not one to three digits (non-zero)
/// and `::`.
fn cloze_header(s: &str) -> Option<(u32, usize)> {
    let after = s.strip_prefix("{{c")?;
    let digits_len = after
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    if digits_len == 0 || digits_len > 3 {
        return None;
    }
    let index: u32 = after[..digits_len].parse().ok()?;
    if index == 0 || !after[digits_len..].starts_with("::") {
        return None;
    }
    Some((index, 3 + digits_len + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_past_the_cap_stays_literal() {
        let mut s = String::new();
        for _ in 0..MAX_NESTING + 2 {
            s.push_str("{{c1::");
        }
        s.push('x');
        for _ in 0..MAX_NESTING + 2 {
            s.push_str("}}");
        }
        let n = parse(&s).unwrap();
        // The innermost two openers were not parsed, so they survive as
        // text in the filled rendering.
        assert!(n.filled(1).contains("{{c1::"));
        assert_eq!(n.indices(), vec![1]);
    }

    #[test]
    fn two_indices_two_cards() {
        let n = parse("{{c1::Ottawa}} is in {{c2::Canada}}").unwrap();
        assert_eq!(n.indices(), vec![1, 2]);
        assert_eq!(n.front(1, false), "[...] is in Canada");
        assert_eq!(n.front(2, false), "Ottawa is in [...]");
        assert_eq!(n.answers(1), vec!["Ottawa"]);
        assert_eq!(n.answers(2), vec!["Canada"]);
    }

    #[test]
    fn hint_renders_in_blank() {
        let n = parse("{{c1::Ottawa::city}} rocks").unwrap();
        assert_eq!(n.front(1, false), "[city] rocks");
        assert_eq!(n.answers(1), vec!["Ottawa"]);
        assert_eq!(
            n.front(1, true),
            r#"<span class="cloze-blank">[city]</span> rocks"#
        );
    }

    #[test]
    fn filled_highlights_the_answer_in_context() {
        let n = parse("{{c1::Ottawa}} is in {{c2::Canada}}").unwrap();
        assert_eq!(
            n.filled(1),
            r#"<span class="cloze-answer">Ottawa</span> is in Canada"#
        );
        assert_eq!(
            n.filled(2),
            r#"Ottawa is in <span class="cloze-answer">Canada</span>"#
        );
    }

    #[test]
    fn multiline_answers_fill_with_a_block_highlight() {
        let n = parse("PROM tests: {{c1::Pooling<br>Nitrazine<br>Ferning}}").unwrap();
        let filled = n.filled(1);
        assert!(
            filled.contains(r#"<div class="cloze-answer-block">Pooling<br>"#),
            "{filled}"
        );
        // Single-line answers keep the inline pill.
        let n = parse("{{c1::one line}}").unwrap();
        assert!(n
            .filled(1)
            .contains(r#"<span class="cloze-answer">one line</span>"#));
    }

    #[test]
    fn same_index_twice_blanks_both() {
        let n = parse("{{c1::a}} x {{c1::b}}").unwrap();
        assert_eq!(n.indices(), vec![1]);
        assert_eq!(n.front(1, false), "[...] x [...]");
        assert_eq!(n.answers(1), vec!["a", "b"]);
    }

    #[test]
    fn nested_clozes() {
        let n = parse("{{c2::A {{c3::B}}}}").unwrap();
        assert_eq!(n.indices(), vec![2, 3]);
        assert_eq!(n.front(2, false), "[...]");
        assert_eq!(n.front(3, false), "A [...]");
        assert_eq!(n.answers(2), vec!["A B"]);
        assert_eq!(n.answers(3), vec!["B"]);
    }

    #[test]
    fn hint_keeps_extra_colons() {
        let n = parse("{{c1::a::b::c}}").unwrap();
        assert_eq!(n.front(1, false), "[b::c]");
        assert_eq!(n.answers(1), vec!["a"]);
    }

    #[test]
    fn malformed_stays_literal() {
        assert!(parse("{{c1::unterminated").is_none());
        assert!(parse("{{c0::zero}}").is_none());
        assert!(parse("{{c::none}}").is_none());
        assert!(parse("no cloze at all").is_none());
        // Valid span after an invalid opener still parses.
        let n = parse("{{cX bad}} then {{c1::good}}").unwrap();
        assert_eq!(n.front(1, false), "{{cX bad}} then [...]");
    }

    #[test]
    fn non_contiguous_index() {
        let n = parse("{{c3::only}}").unwrap();
        assert_eq!(n.indices(), vec![3]);
    }
}
