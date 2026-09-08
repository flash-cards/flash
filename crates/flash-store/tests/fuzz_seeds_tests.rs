//! The fuzz corpus the CI job starts from lives in `fuzz/seeds/<target>`
//! and is checked in, because a fuzzer that starts from empty bytes
//! never synthesizes a zip around a SQLite file around a protobuf
//! manifest, and so never reaches the parsers it is meant to exercise.
//! This test keeps the seeds honest: every seed must exist and must be
//! accepted by the importer, and the seeds are regenerated from the
//! exporter (`FLASH_WRITE_FUZZ_SEEDS=1 cargo test -p flash-store --test
//! fuzz_seeds_tests`) so they track the formats the code writes.

use std::io::Write;
use std::path::{Path, PathBuf};

use flash_store::export::{build_apkg, ExportCard, ExportMedia};
use flash_store::import::{parse_apkg, read_media_manifest};

const NOW: i64 = 1_700_000_000_000;

fn seeds_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/apkg")
}

fn cards() -> Vec<ExportCard> {
    vec![
        ExportCard {
            deck: "Seed".into(),
            front: "Warfarin antidote?".into(),
            back: "Vitamin <b>K</b>".into(),
            tags: vec!["exam-2".into()],
            ..Default::default()
        },
        ExportCard {
            deck: "Seed".into(),
            front: "{{c1::Paris}} is in {{c2::France}}".into(),
            back: "Extra".into(),
            ..Default::default()
        },
    ]
}

/// The exporter's legacy package: `collection.anki2`, JSON manifest.
fn legacy_seed() -> Vec<u8> {
    build_apkg(&cards(), &ExportMedia::default(), NOW).unwrap()
}

/// The same collection in the modern layout: `meta` saying version 3,
/// `collection.anki21b` zstd-compressed, and a protobuf `MediaEntries`
/// manifest (one entry) zstd-compressed, with the media file itself.
fn modern_seed() -> Vec<u8> {
    let legacy = legacy_seed();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&legacy)).unwrap();
    let mut collection = Vec::new();
    std::io::Read::read_to_end(
        &mut zip.by_name("collection.anki2").unwrap(),
        &mut collection,
    )
    .unwrap();

    let media_file = b"GIF89a\x01\x00\x01\x00\x80\x00\x00";
    let sha1: [u8; 20] = {
        let mut h = sha1_smol::Sha1::new();
        h.update(media_file);
        h.digest().bytes()
    };
    // MediaEntries { entries: [ MediaEntry { name: "x.gif", size, sha1 } ] }
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0x0A, 5]);
    entry.extend_from_slice(b"x.gif");
    entry.extend_from_slice(&[0x10, media_file.len() as u8]);
    entry.extend_from_slice(&[0x1A, 20]);
    entry.extend_from_slice(&sha1);
    let mut manifest = vec![0x0A, entry.len() as u8];
    manifest.extend_from_slice(&entry);

    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    out.start_file("meta", options).unwrap();
    out.write_all(&[0x08, 0x03]).unwrap();
    out.start_file("media", options).unwrap();
    out.write_all(&zstd::encode_all(&manifest[..], 3).unwrap())
        .unwrap();
    out.start_file("collection.anki21b", options).unwrap();
    out.write_all(&zstd::encode_all(&collection[..], 3).unwrap())
        .unwrap();
    out.start_file("0", options).unwrap();
    out.write_all(&zstd::encode_all(&media_file[..], 3).unwrap())
        .unwrap();
    out.finish().unwrap().into_inner()
}

type Seed = (&'static str, fn() -> Vec<u8>);

#[test]
fn the_fuzz_seeds_exist_and_the_importer_accepts_them() {
    let dir = seeds_dir();
    let seeds: [Seed; 2] = [("legacy.apkg", legacy_seed), ("modern.apkg", modern_seed)];
    if std::env::var("FLASH_WRITE_FUZZ_SEEDS").is_ok() {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, build) in seeds {
            std::fs::write(dir.join(name), build()).unwrap();
        }
    }
    for (name, _) in seeds {
        let path = dir.join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (regenerate with FLASH_WRITE_FUZZ_SEEDS=1)",
                path.display()
            )
        });
        let parsed = parse_apkg(&bytes).unwrap_or_else(|e| panic!("{name}: {e} / {}", e.detail()));
        assert!(parsed.rows.len() >= 2, "{name}: {} rows", parsed.rows.len());
    }
    // The modern seed reaches the protobuf manifest and the zstd entry.
    let modern = std::fs::read(dir.join("modern.apkg")).unwrap();
    let entries = read_media_manifest(&modern).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].filename, "x.gif");

    // And the seeds are what the generator makes today: a format change
    // the generator follows must be written back into the corpus. Byte
    // equality would tie the seed to SQLite's page layout; the structure
    // is what matters: the same rows, in the same order.
    let a = parse_apkg(&modern).unwrap();
    let b = parse_apkg(&modern_seed()).unwrap();
    assert_eq!(
        a.rows.iter().map(|r| &r.front).collect::<Vec<_>>(),
        b.rows.iter().map(|r| &r.front).collect::<Vec<_>>(),
        "regenerate with FLASH_WRITE_FUZZ_SEEDS=1"
    );
}
