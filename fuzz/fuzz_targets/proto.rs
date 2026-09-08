//! The importer's binary decoders on their own bytes: the protobuf
//! media manifest, the legacy JSON manifest, a notetype's config, a
//! deck's options, and the varint/length primitives under them. These
//! sit behind zip, zstd and SQLite in a real package, which a raw-bytes
//! fuzzer never gets through; here they are the whole input. Nothing may
//! panic: a length the file chose is added checked, and a slice past the
//! end is a `None`, not an index out of range.

#![no_main]

use flash_store::import::fuzz::{
    deck_config, media_entries_proto, media_map_json, notetype_config, walk_fields,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = media_entries_proto(data);
    let _ = media_map_json(data);
    let _ = notetype_config(data);
    let _ = deck_config(data);
    walk_fields(data);
});
