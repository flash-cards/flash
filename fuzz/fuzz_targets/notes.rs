//! A note's two fields through card generation for every note type,
//! which is where the cloze parser (nesting, hints, indices) and the
//! per-field HTML sanitizer meet arbitrary editor input.

#![no_main]

use flash_store::notes::{generate_cards, NoteType};
use flash_store::richtext::sanitize_with_media;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let (front, back) = s.split_once('\x1f').unwrap_or((s, ""));
    // Card generation takes sanitized fields by type, exactly as the
    // editor path hands them over.
    let (front, back) = (sanitize_with_media(front), sanitize_with_media(back));
    for kind in NoteType::ALL {
        let _ = generate_cards(kind, &front, &back);
    }
});
