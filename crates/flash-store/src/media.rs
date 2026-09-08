//! Media plumbing shared by import parsing, storage, and serving: what
//! counts as media, how a filename maps to a kind/MIME, how card HTML
//! references media, and the security checks every ingested file must pass.
//!
//! Threat model: an .apkg is an attacker-controlled zip. Filenames may try
//! path traversal; bytes may lie about their extension (HTML-as-.jpg gives
//! stored XSS when served from our origin); SVG is scriptable and rejected
//! outright. Blobs are stored content-addressed (sha256 as the disk name),
//! so a hostile filename never touches the filesystem.

use std::collections::HashSet;

/// Largest single media file accepted — AnkiWeb's own per-file limit, so
/// anything Anki syncs, Flash accepts.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024; // 100 MiB

/// Media has no advertised total (again matching AnkiWeb). This is only a
/// per-user abuse valve: ingest refuses past it and logs, nothing in the
/// UI ever shows it.
pub const SOFT_CAP_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    /// Recognized by Anki but not accepted by Flash (e.g. SVG, unknown).
    Unsupported,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Unsupported => "unsupported",
        }
    }

    /// The inverse of `as_str` for a stored column; anything unknown is
    /// `Unsupported`.
    pub fn from_db(s: &str) -> Self {
        match s {
            "image" => Self::Image,
            "audio" => Self::Audio,
            "video" => Self::Video,
            _ => Self::Unsupported,
        }
    }
}

/// One media reference found in a card field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MediaRef {
    pub filename: String,
    pub kind: MediaKind,
}

/// Extension -> (kind, mime). SVG is deliberately Unsupported: it can carry
/// scripts and served inline it would run in our origin. HTML-ish and
/// unknown extensions are Unsupported too.
pub fn classify(filename: &str) -> (MediaKind, &'static str) {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => (MediaKind::Image, "image/jpeg"),
        "png" => (MediaKind::Image, "image/png"),
        "gif" => (MediaKind::Image, "image/gif"),
        "webp" => (MediaKind::Image, "image/webp"),
        "avif" => (MediaKind::Image, "image/avif"),
        "bmp" => (MediaKind::Image, "image/bmp"),
        "mp3" => (MediaKind::Audio, "audio/mpeg"),
        "ogg" | "oga" | "opus" | "spx" => (MediaKind::Audio, "audio/ogg"),
        "wav" => (MediaKind::Audio, "audio/wav"),
        "m4a" => (MediaKind::Audio, "audio/mp4"),
        "aac" => (MediaKind::Audio, "audio/aac"),
        "flac" => (MediaKind::Audio, "audio/flac"),
        "3gp" => (MediaKind::Audio, "audio/3gpp"),
        "mp4" | "m4v" => (MediaKind::Video, "video/mp4"),
        "webm" => (MediaKind::Video, "video/webm"),
        "mkv" => (MediaKind::Video, "video/x-matroska"),
        "mov" => (MediaKind::Video, "video/quicktime"),
        "ogv" => (MediaKind::Video, "video/ogg"),
        _ => (MediaKind::Unsupported, "application/octet-stream"),
    }
}

/// Does the file content plausibly match its claimed kind? Magic-byte
/// check so a .jpg that is really HTML/script never gets stored or served.
/// Container formats overlap (ogg/mp4/riff hold audio or video); we accept
/// either side of those pairs — the point is rejecting non-media bytes,
/// not perfect demuxing.
pub fn content_matches(kind: MediaKind, bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let riff = &bytes[..4] == b"RIFF";
    let ogg = &bytes[..4] == b"OggS";
    let ftyp = &bytes[4..8] == b"ftyp";
    let ebml = bytes[..4] == [0x1A, 0x45, 0xDF, 0xA3]; // webm/mkv
    match kind {
        MediaKind::Image => {
            bytes.starts_with(&[0xFF, 0xD8, 0xFF]) // jpeg
                || bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
                || bytes.starts_with(b"GIF87a")
                || bytes.starts_with(b"GIF89a")
                || (riff && &bytes[8..12] == b"WEBP")
                || ftyp // avif
                || bytes.starts_with(b"BM") // bmp
        }
        MediaKind::Audio => {
            bytes.starts_with(b"ID3")
                || (bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0) // mp3 frame sync
                || ogg
                || (riff && &bytes[8..12] == b"WAVE")
                || bytes.starts_with(b"fLaC")
                || ftyp // m4a/3gp
        }
        MediaKind::Video => ftyp || ebml || ogg,
        MediaKind::Unsupported => false,
    }
}

/// Display-safe filename: path bits stripped, control chars and quotes
/// removed, length capped. Never used as a disk path — blobs live under
/// their sha256 — this is metadata for the UI and Content-Disposition.
pub fn sanitize_filename(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    // Cap by characters, never by bytes: `String::truncate` panics inside
    // a multibyte character, and filenames arrive from uploads and .apkg
    // manifests.
    let out: String = leaf
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '<' | '>'))
        .take(120)
        .collect();
    let trimmed = out.trim().trim_start_matches('.').to_string();
    if trimmed.is_empty() {
        "file".into()
    } else {
        trimmed
    }
}

/// Scans one field's raw HTML for media references, Anki-style:
/// `[sound:file]` tags (audio *and* video files travel in these) and
/// `src`/`data` attributes on img/audio/video/source/object/embed tags.
/// External (`http:`, `https:`, `data:`) references are skipped — they are
/// not files in the package. Percent-encoding and basic entities in
/// attribute values are decoded so names match the media manifest.
pub fn scan_refs(html: &str) -> Vec<MediaRef> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut push = |raw: &str| {
        let name = decode_entities(&percent_decode(raw.trim()));
        if name.is_empty() || name.contains("://") || name.starts_with("data:") {
            return;
        }
        if seen.insert(name.clone()) {
            let (kind, _) = classify(&name);
            out.push(MediaRef {
                filename: name,
                kind,
            });
        }
    };

    // [sound:...] tags.
    let mut rest = html;
    while let Some(start) = rest.find("[sound:") {
        let after = &rest[start + 7..];
        match after.find(']') {
            Some(end) => {
                push(&after[..end]);
                rest = &after[end + 1..];
            }
            None => break,
        }
    }

    // src=/data= attributes on media tags.
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        let tag = &rest[lt + 1..];
        let end = tag.find('>').unwrap_or(tag.len());
        let body = &tag[..end];
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let tag_name = body[..name_end].to_ascii_lowercase();
        if matches!(
            tag_name.as_str(),
            "img" | "audio" | "video" | "source" | "object" | "embed"
        ) {
            for attr in ["src", "data"] {
                if let Some(value) = attr_value(body, attr) {
                    push(value);
                }
            }
        }
        rest = &tag[end..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// Fully decodes a raw media reference (percent-encoding + entities).
pub(crate) fn decode_ref(raw: &str) -> String {
    decode_entities(&percent_decode(raw.trim()))
}

/// Lowercased element name of a tag body (`/` for closing tags skipped):
/// `"IMG src=x"` and `"/img"` both give `"img"`.
pub(crate) fn tag_name(body: &str) -> String {
    body.trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Value of `attr=` inside a tag body: quoted (single/double) or bare.
///
/// A real attribute walk, not a substring search: the body is read as
/// `name`, `name=value`, `name="value"`, `name='value'` in turn, so an
/// `attr=` that sits *inside* another attribute's quoted value is never
/// mistaken for the attribute itself. The first attribute with that name
/// and a value wins.
pub(crate) fn attr_value<'a>(tag_body: &'a str, attr: &str) -> Option<&'a str> {
    let bytes = tag_body.as_bytes();
    let is_space = |b: u8| b.is_ascii_whitespace();
    let mut i = 0;
    // The element name.
    while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'/' {
        i += 1;
    }
    loop {
        while i < bytes.len() && (is_space(bytes[i]) || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let name_start = i;
        while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        // Every cut above lands on an ASCII byte or the end, so the slice
        // is on a char boundary whatever the name contains.
        let name = &tag_body[name_start..i];
        while i < bytes.len() && is_space(bytes[i]) {
            i += 1;
        }
        let mut value: Option<&str> = None;
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && is_space(bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = Some(&tag_body[start..i]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let start = i;
                while i < bytes.len() && !is_space(bytes[i]) {
                    i += 1;
                }
                value = Some(&tag_body[start..i]);
            }
        }
        if name.eq_ignore_ascii_case(attr) {
            if let Some(value) = value {
                return Some(value);
            }
        }
    }
}

/// Minimal %XX decoding (Anki writes e.g. spaces as %20 in field HTML).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Work on bytes: the two characters after '%' may not be ASCII,
        // and slicing the &str there would panic on a char boundary.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &bytes[i + 1..i + 3];
            if hex.iter().all(u8::is_ascii_hexdigit) {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("zz"), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_common_formats() {
        assert_eq!(classify("a.JPG"), (MediaKind::Image, "image/jpeg"));
        assert_eq!(classify("b.webp").0, MediaKind::Image);
        assert_eq!(classify("c.mp3").0, MediaKind::Audio);
        assert_eq!(classify("d.opus").0, MediaKind::Audio);
        assert_eq!(classify("e.mp4").0, MediaKind::Video);
        assert_eq!(classify("f.webm").0, MediaKind::Video);
        // Scriptable or unknown -> unsupported.
        assert_eq!(classify("evil.svg").0, MediaKind::Unsupported);
        assert_eq!(classify("evil.html").0, MediaKind::Unsupported);
        assert_eq!(classify("noext").0, MediaKind::Unsupported);
    }

    #[test]
    fn content_check_blocks_masquerading_html() {
        let html = b"<html><script>alert(1)</script></html>";
        assert!(!content_matches(MediaKind::Image, html));
        assert!(!content_matches(MediaKind::Audio, html));
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        assert!(content_matches(MediaKind::Image, &png));
        let mut ogg = b"OggS".to_vec();
        ogg.extend_from_slice(&[0; 8]);
        assert!(content_matches(MediaKind::Audio, &ogg));
    }

    #[test]
    fn sanitize_strips_paths_and_junk() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        assert_eq!(sanitize_filename("a\"b<c>.png"), "abc.png");
        assert_eq!(sanitize_filename(""), "file");
    }

    #[test]
    fn scan_finds_sound_and_img_refs() {
        let html = r#"Hello [sound:word ja.mp3] <img src="dog%20run.jpg"> and
            <img src='x.png'/> <video src=clip.mp4></video>"#;
        let refs = scan_refs(html);
        let names: Vec<&str> = refs.iter().map(|r| r.filename.as_str()).collect();
        assert_eq!(
            names,
            vec!["word ja.mp3", "dog run.jpg", "x.png", "clip.mp4"]
        );
        assert_eq!(refs[0].kind, MediaKind::Audio);
        assert_eq!(refs[1].kind, MediaKind::Image);
        assert_eq!(refs[3].kind, MediaKind::Video);
    }

    #[test]
    fn scan_skips_external_and_data_urls() {
        let html = r#"<img src="https://x.test/a.png"> <img src="data:image/png;base64,AAAA">
            [sound:https://x.test/b.mp3]"#;
        assert!(scan_refs(html).is_empty());
    }

    #[test]
    fn scan_dedupes_repeated_refs() {
        let refs = scan_refs(r#"<img src="a.png"><img src="a.png">[sound:a.png]"#);
        assert_eq!(refs.len(), 1);
    }
}
