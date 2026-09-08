//! A modern .apkg whose container is built here around the fuzzed
//! bytes: the fuzzer chooses the media manifest and the collection
//! file, the harness supplies the zip, the zstd frames and a `meta`
//! entry that says "version 3", so the bytes reach the protobuf and
//! SQLite readers on every iteration instead of failing at the zip
//! header. The first byte picks the layout: bit 0 zstd-compresses the
//! collection (the `.anki21b` form), bit 1 marks the manifest as
//! protobuf (zstd) rather than legacy JSON, bit 2 adds one media file.
//! The next two bytes are a big-endian split point between manifest
//! and collection.

#![no_main]

use std::io::Write;

use flash_store::import::{extract_media_file, parse_apkg, read_media_manifest};
use libfuzzer_sys::fuzz_target;

fn package(layout: u8, manifest: &[u8], collection: &[u8]) -> Vec<u8> {
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let zstd_collection = layout & 1 != 0;
    let proto_manifest = layout & 2 != 0;
    let with_media = layout & 4 != 0;

    // PackageMetadata { version: 3 } tells the reader to expect zstd
    // and protobuf; version 2 (legacy) is the JSON manifest.
    let version = if proto_manifest { 3u8 } else { 2u8 };
    let _ = out.start_file("meta", options);
    let _ = out.write_all(&[0x08, version]);

    let manifest_bytes = if proto_manifest {
        zstd::encode_all(manifest, 1).unwrap_or_default()
    } else {
        manifest.to_vec()
    };
    let _ = out.start_file("media", options);
    let _ = out.write_all(&manifest_bytes);

    let (name, bytes) = if zstd_collection {
        (
            "collection.anki21b",
            zstd::encode_all(collection, 1).unwrap_or_default(),
        )
    } else {
        ("collection.anki2", collection.to_vec())
    };
    let _ = out.start_file(name, options);
    let _ = out.write_all(&bytes);

    if with_media {
        let head = &manifest[..manifest.len().min(64)];
        let media: Vec<u8> = if proto_manifest {
            zstd::encode_all(head, 1).unwrap_or_default()
        } else {
            head.to_vec()
        };
        let _ = out.start_file("0", options);
        let _ = out.write_all(&media);
    }
    out.finish().map(|c| c.into_inner()).unwrap_or_default()
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let layout = data[0];
    let split = (u16::from_be_bytes([data[1], data[2]]) as usize).min(data.len() - 3);
    let (manifest, collection) = data[3..].split_at(split);
    let bytes = package(layout, manifest, collection);
    let _ = parse_apkg(&bytes);
    if let Ok(entries) = read_media_manifest(&bytes) {
        for entry in entries.iter().take(16) {
            let _ = extract_media_file(&bytes, entry, 1 << 16);
        }
    }
});
