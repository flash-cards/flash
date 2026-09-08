//! The media side of an .apkg: numbered zip entries ("0", "1", ...) plus a
//! `media` manifest mapping them to real filenames. Two generations exist:
//!
//! - Legacy (collection.anki2/.anki21, no `meta` file): `media` is plain
//!   JSON `{"0": "cat.jpg", ...}`; file entries are raw (zip deflate only).
//! - Latest (`meta` = protobuf PackageMetadata with version 3,
//!   collection.anki21b): `media` is a zstd-compressed protobuf
//!   `MediaEntries` (name, size, sha1 per entry, list index = zip name);
//!   each numbered file entry is itself zstd-compressed.
//!
//! Everything here treats the package as attacker-controlled: manifest and
//! file decompression are hard-capped (zstd/deflate bombs), filenames are
//! sanitized, and sha1 declared in the manifest is verified on extraction.

use std::io::Read;

use super::proto::{read_varint, skip_field};
use super::ImportError;
use crate::media::{classify, sanitize_filename, MediaKind};

/// The uploader's sentences; library detail goes to `ImportError::detail`.
const NOT_A_ZIP: &str = "that file is not an Anki package (not a zip archive)";
const MANIFEST_DAMAGED: &str = "the package's media list is damaged and could not be read";
const COMPRESSED_DAMAGED: &str = "compressed content is damaged and could not be unpacked";

/// Decompressed manifest cap: a manifest bigger than this is not a deck.
const MANIFEST_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ApkgMediaEntry {
    /// Name of the zip entry holding the bytes ("0", "1", ...).
    pub zip_name: String,
    /// Sanitized real filename, as referenced from note fields.
    pub filename: String,
    pub kind: MediaKind,
    /// Uncompressed size: declared by the manifest (latest) or taken from
    /// the zip header (legacy). Untrusted until extraction verifies.
    pub size: u64,
    /// Declared sha1 of the uncompressed bytes (latest format only).
    pub sha1: Option<[u8; 20]>,
    /// Whether the zip entry bytes are zstd-compressed (latest format).
    pub zstd: bool,
}

/// The one way into a package's archive. The central directory is
/// parsed in memory when the archive opens, so the entry count is
/// checked here, before any caller looks inside; every reader of the
/// package (the collection, the manifest, each media file) comes
/// through this function.
pub(crate) fn open_package(
    bytes: &[u8],
) -> Result<zip::ZipArchive<std::io::Cursor<&[u8]>>, ImportError> {
    let zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ImportError::new(NOT_A_ZIP, format!("zip: {e}")))?;
    if zip.len() > super::MAX_PACKAGE_ENTRIES {
        return Err(ImportError::user(format!(
            "that package holds more than {} files; imports are capped at that many per file",
            super::MAX_PACKAGE_ENTRIES
        )));
    }
    Ok(zip)
}

/// Reads the media manifest. Packages without media (or without a `media`
/// entry at all) yield an empty list; a malformed manifest is an error.
pub fn read_media_manifest(bytes: &[u8]) -> Result<Vec<ApkgMediaEntry>, ImportError> {
    let mut zip = open_package(bytes)?;
    let latest = package_is_latest(&mut zip);

    let mut manifest_bytes = Vec::new();
    match zip.by_name("media") {
        Ok(file) => {
            file.take(MANIFEST_MAX_BYTES + 1)
                .read_to_end(&mut manifest_bytes)
                .map_err(|e| ImportError::new(MANIFEST_DAMAGED, format!("read: {e}")))?;
            if manifest_bytes.len() as u64 > MANIFEST_MAX_BYTES {
                return Err(ImportError::user("the package's media list is too large"));
            }
        }
        Err(_) => return Ok(Vec::new()),
    }

    let mut entries = if latest {
        let decoded = zstd_decode_capped(&manifest_bytes, MANIFEST_MAX_BYTES)
            .map_err(|e| ImportError::new(MANIFEST_DAMAGED, e.detail()))?;
        parse_media_entries_proto(&decoded)
    } else {
        parse_media_map_json(&manifest_bytes)?
    };

    // Legacy manifests carry no sizes; fill from the zip headers.
    for entry in &mut entries {
        if entry.size == 0 {
            if let Ok(file) = zip.by_name(&entry.zip_name) {
                entry.size = file.size();
            }
        }
    }
    // Drop manifest rows whose zip entry doesn't exist.
    let names: std::collections::HashSet<String> = zip.file_names().map(str::to_string).collect();
    entries.retain(|e| names.contains(&e.zip_name));
    Ok(entries)
}

/// Extracts one media file, enforcing `max_bytes` on the decompressed
/// output (bomb defense) and verifying the manifest sha1 when present.
pub fn extract_media_file(
    bytes: &[u8],
    entry: &ApkgMediaEntry,
    max_bytes: u64,
) -> Result<Vec<u8>, ImportError> {
    let mut zip = open_package(bytes)?;
    let file = zip.by_name(&entry.zip_name).map_err(|_| {
        ImportError::new(
            format!("{} is missing from the package", entry.filename),
            format!("entry {} absent", entry.zip_name),
        )
    })?;
    let mut raw = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut raw)
        .map_err(|e| {
            ImportError::new(
                format!("{} could not be read from the package", entry.filename),
                format!("entry {}: {e}", entry.zip_name),
            )
        })?;
    if raw.len() as u64 > max_bytes {
        return Err(ImportError::user(format!(
            "{}: larger than the {max_bytes} byte limit",
            entry.filename
        )));
    }
    let out = if entry.zstd {
        zstd_decode_capped(&raw, max_bytes).map_err(|e| {
            ImportError::new(
                format!("{}: {}", entry.filename, e.public()),
                format!("entry {}: {}", entry.zip_name, e.detail()),
            )
        })?
    } else {
        raw
    };
    if let Some(expected) = entry.sha1 {
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(&out);
        if hasher.digest().bytes() != expected {
            return Err(ImportError::user(format!(
                "{}: content does not match its declared hash",
                entry.filename
            )));
        }
    }
    Ok(out)
}

/// zstd decode with a hard output cap; errors instead of ballooning.
pub(super) fn zstd_decode_capped(bytes: &[u8], max_bytes: u64) -> Result<Vec<u8>, ImportError> {
    let damaged = |e: std::io::Error| ImportError::new(COMPRESSED_DAMAGED, format!("zstd: {e}"));
    let decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(bytes)).map_err(damaged)?;
    let mut out = Vec::new();
    decoder
        .take(max_bytes + 1)
        .read_to_end(&mut out)
        .map_err(damaged)?;
    if out.len() as u64 > max_bytes {
        return Err(ImportError::user(format!(
            "decompresses past the {max_bytes} byte limit"
        )));
    }
    Ok(out)
}

/// Latest format detection: a `meta` entry whose PackageMetadata.version
/// (protobuf field 1, varint) is 3+. Absent/unreadable meta = legacy.
fn package_is_latest(zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>) -> bool {
    let Ok(file) = zip.by_name("meta") else {
        return false;
    };
    let mut buf = Vec::new();
    if file.take(64).read_to_end(&mut buf).is_err() {
        return false;
    }
    let mut i = 0;
    while i < buf.len() {
        let Some(tag) = read_varint(&buf, &mut i) else {
            return false;
        };
        match (tag >> 3, tag & 7) {
            (1, 0) => return read_varint(&buf, &mut i).is_some_and(|v| v >= 3),
            (_, wire) => {
                if !skip_field(&buf, &mut i, wire) {
                    return false;
                }
            }
        }
    }
    false
}

/// Legacy JSON map: {"0": "real name.jpg", ...} (zip entry -> filename).
pub(crate) fn parse_media_map_json(bytes: &[u8]) -> Result<Vec<ApkgMediaEntry>, ImportError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ImportError::new(MANIFEST_DAMAGED, format!("json: {e}")))?;
    let Some(map) = value.as_object() else {
        return Err(ImportError::new(
            MANIFEST_DAMAGED,
            "media manifest is not a JSON object",
        ));
    };
    let mut out = Vec::new();
    for (zip_name, name) in map {
        let Some(name) = name.as_str() else { continue };
        let filename = sanitize_filename(name);
        let (kind, _) = classify(&filename);
        out.push(ApkgMediaEntry {
            zip_name: zip_name.clone(),
            filename,
            kind,
            size: 0,
            sha1: None,
            zstd: false,
        });
    }
    Ok(out)
}

/// Minimal decoder for Anki's MediaEntries protobuf:
///   MediaEntries { repeated MediaEntry entries = 1; }
///   MediaEntry { string name = 1; uint32 size = 2; bytes sha1 = 3;
///                optional uint32 legacy_zip_filename = 255; }
/// The zip entry name is the list index unless legacy_zip_filename says
/// otherwise. Unknown/short input degrades to fewer entries, never panics.
pub(crate) fn parse_media_entries_proto(bytes: &[u8]) -> Vec<ApkgMediaEntry> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut index: u64 = 0;
    while i < bytes.len() {
        let Some(tag) = read_varint(bytes, &mut i) else {
            break;
        };
        if tag >> 3 == 1 && tag & 7 == 2 {
            let Some(len) = read_varint(bytes, &mut i) else {
                break;
            };
            // The length is the file's word; added unchecked it could wrap
            // to an `end` before `i` that passes the bounds test below.
            let Some(end) = usize::try_from(len).ok().and_then(|l| i.checked_add(l)) else {
                break;
            };
            if end > bytes.len() {
                break;
            }
            if let Some(entry) = parse_media_entry(&bytes[i..end], index) {
                out.push(entry);
            }
            i = end;
            index += 1;
        } else if !skip_field(bytes, &mut i, tag & 7) {
            break;
        }
    }
    out
}

fn parse_media_entry(bytes: &[u8], index: u64) -> Option<ApkgMediaEntry> {
    let mut name = String::new();
    let mut size: u64 = 0;
    let mut sha1: Option<[u8; 20]> = None;
    let mut zip_index = index;
    let mut i = 0;
    while i < bytes.len() {
        let tag = read_varint(bytes, &mut i)?;
        match (tag >> 3, tag & 7) {
            (1, 2) => {
                let len = read_varint(bytes, &mut i)? as usize;
                let end = i.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }
                name = String::from_utf8_lossy(&bytes[i..end]).into_owned();
                i = end;
            }
            (2, 0) => size = read_varint(bytes, &mut i)?,
            (3, 2) => {
                let len = read_varint(bytes, &mut i)? as usize;
                let end = i.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }
                if len == 20 {
                    let mut buf = [0u8; 20];
                    buf.copy_from_slice(&bytes[i..end]);
                    sha1 = Some(buf);
                }
                i = end;
            }
            (255, 0) => zip_index = read_varint(bytes, &mut i)?,
            (_, wire) => {
                if !skip_field(bytes, &mut i, wire) {
                    return None;
                }
            }
        }
    }
    if name.is_empty() {
        return None;
    }
    let filename = sanitize_filename(&name);
    let (kind, _) = classify(&filename);
    Some(ApkgMediaEntry {
        zip_name: zip_index.to_string(),
        filename,
        kind,
        size,
        sha1,
        zstd: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-encodes a MediaEntry submessage.
    fn encode_entry(name: &str, size: u64, sha1: Option<&[u8; 20]>) -> Vec<u8> {
        let mut e = Vec::new();
        e.push(0x0A); // field 1, wire 2
        e.push(name.len() as u8);
        e.extend_from_slice(name.as_bytes());
        e.push(0x10); // field 2, wire 0
        encode_varint(size, &mut e);
        if let Some(h) = sha1 {
            e.push(0x1A); // field 3, wire 2
            e.push(20);
            e.extend_from_slice(h);
        }
        let mut out = Vec::new();
        out.push(0x0A); // entries: field 1, wire 2
        out.push(e.len() as u8);
        out.extend_from_slice(&e);
        out
    }

    fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn proto_media_entries_decode() {
        let mut buf = encode_entry("cat.jpg", 1234, Some(&[7u8; 20]));
        buf.extend(encode_entry("word.mp3", 999, None));
        let entries = parse_media_entries_proto(&buf);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].filename, "cat.jpg");
        assert_eq!(entries[0].zip_name, "0");
        assert_eq!(entries[0].size, 1234);
        assert_eq!(entries[0].sha1, Some([7u8; 20]));
        assert_eq!(entries[1].filename, "word.mp3");
        assert_eq!(entries[1].zip_name, "1");
        assert!(entries[1].zstd);
    }

    #[test]
    fn proto_garbage_degrades_gracefully() {
        assert!(parse_media_entries_proto(&[0xFF, 0xFF, 0xFF]).is_empty());
        assert!(parse_media_entries_proto(&[]).is_empty());
    }

    #[test]
    fn zstd_cap_stops_bombs() {
        // 10 MiB of zeros compresses tiny; a 1 KiB cap must reject it.
        let big = vec![0u8; 10 * 1024 * 1024];
        let compressed = zstd::encode_all(std::io::Cursor::new(big), 3).unwrap();
        assert!(compressed.len() < 20_000);
        let err = zstd_decode_capped(&compressed, 1024).unwrap_err();
        assert!(err.public().contains("limit"), "{err}");
    }

    #[test]
    fn json_manifest_sanitizes_traversal_names() {
        let entries =
            parse_media_map_json(br#"{"0": "../../etc/cron.d/evil", "1": "ok.png"}"#).unwrap();
        let evil = entries.iter().find(|e| e.zip_name == "0").unwrap();
        assert_eq!(evil.filename, "evil");
        assert_eq!(evil.kind, MediaKind::Unsupported);
        let ok = entries.iter().find(|e| e.zip_name == "1").unwrap();
        assert_eq!(ok.kind, MediaKind::Image);
    }
}
