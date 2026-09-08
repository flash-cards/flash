//! An uploaded .apkg: the zip, the collection inside it (legacy and
//! zstd-compressed), the media manifest and each media entry. Nothing in
//! here may panic or hang; every failure is an `Err` the importer shows.

#![no_main]

use flash_store::import::{extract_media_file, parse_apkg, read_media_manifest};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_apkg(data);
    if let Ok(entries) = read_media_manifest(data) {
        for entry in entries.iter().take(64) {
            let _ = extract_media_file(data, entry, 1 << 20);
        }
    }
});
