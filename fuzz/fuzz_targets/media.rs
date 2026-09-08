//! An upload's filename and bytes: name sanitizing, kind and MIME from
//! the extension, the magic-byte check against that kind, and the media
//! reference scan over card HTML.

#![no_main]

use flash_store::media::{classify, content_matches, sanitize_filename, scan_refs, MediaKind};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first line is the filename, the rest the file (or the HTML).
    let split = data.iter().position(|b| *b == b'\n').unwrap_or(data.len());
    let name = String::from_utf8_lossy(&data[..split]);
    let body = &data[split..];

    let clean = sanitize_filename(&name);
    let (kind, _mime) = classify(&clean);
    let _ = content_matches(kind, body);
    let _ = content_matches(MediaKind::from_db(&name), body);
    if let Ok(html) = std::str::from_utf8(body) {
        let _ = scan_refs(html);
    }
});
