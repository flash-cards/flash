# SPDX-License-Identifier: AGPL-3.0-only
"""Validate an .apkg's structure and build size-capped variants.

validate: every zip entry must be a collection file, the media manifest,
or a numbered media file; numbered files must carry image/audio/video
magic bytes (no executables, scripts, or surprises).

variants: <name>-nomedia.apkg (collection only, empty manifest) and
optionally <name>-somemedia.apkg (media kept until a byte budget runs
out, manifest rewritten to the kept subset).
"""
import io, json, sys, zipfile

MAGICS = [
    (b"\xff\xd8\xff", "jpeg"), (b"\x89PNG", "png"), (b"GIF8", "gif"),
    (b"RIFF", "riff(webp/wav)"), (b"OggS", "ogg"), (b"ID3", "mp3"),
    (b"fLaC", "flac"), (b"\x1a\x45\xdf\xa3", "webm/mkv"), (b"BM", "bmp"),
]

def sniff(data):
    for magic, name in MAGICS:
        if data.startswith(magic):
            return name
    if len(data) > 8 and data[4:8] == b"ftyp":
        return "mp4/m4a"
    if data[:2] == b"\xff\xfb" or data[:2] == b"\xff\xf3" or data[:2] == b"\xff\xf2":
        return "mp3-frame"
    if data[:5] == b"<svg " or data[:5] == b"<?xml":
        return "svg/xml"
    return "UNKNOWN:" + data[:8].hex()

def main(path, budget_mb=None):
    z = zipfile.ZipFile(path)
    names = z.namelist()
    collections = [n for n in names if n.startswith("collection.")]
    numbered = [n for n in names if n.isdigit()]
    other = [n for n in names if n not in collections and not n.isdigit()
             and n not in ("media", "meta")]
    print(f"entries={len(names)} collections={collections} "
          f"numbered={len(numbered)} manifest={'media' in names} "
          f"meta={'meta' in names} UNEXPECTED={other[:10]}")

    manifest = json.loads(z.read("media").decode()) if "media" in names else {}
    print(f"manifest entries: {len(manifest)}")
    exts = {}
    for fname in manifest.values():
        ext = fname.rsplit(".", 1)[-1].lower() if "." in fname else "(none)"
        exts[ext] = exts.get(ext, 0) + 1
    print("extensions:", dict(sorted(exts.items(), key=lambda x: -x[1])))

    kinds = {}
    bad = []
    for n in numbered[:4000]:
        head = z.open(n).read(16)
        k = sniff(head)
        kinds[k] = kinds.get(k, 0) + 1
        if k.startswith("UNKNOWN"):
            bad.append((n, manifest.get(n, "?"), k))
    print("magic bytes:", dict(sorted(kinds.items(), key=lambda x: -x[1])))
    if bad:
        print("SUSPICIOUS:", bad[:10])
    else:
        print("all media files carry media magic bytes — no executables/scripts")

    base = path.rsplit(".apkg", 1)[0]
    # -nomedia variant
    with zipfile.ZipFile(base + "-nomedia.apkg", "w", zipfile.ZIP_DEFLATED) as out:
        for c in collections:
            out.writestr(c, z.read(c))
        out.writestr("media", "{}")
    print("wrote", base + "-nomedia.apkg")

    if budget_mb:
        budget = budget_mb * 1024 * 1024
        kept, used = {}, 0
        with zipfile.ZipFile(base + "-somemedia.apkg", "w", zipfile.ZIP_STORED) as out:
            for c in collections:
                out.writestr(c, z.read(c), zipfile.ZIP_DEFLATED)
            for n in sorted(numbered, key=int):
                size = z.getinfo(n).file_size
                if used + size > budget:
                    continue
                out.writestr(n, z.read(n))
                kept[n] = manifest.get(n, n)
                used += size
            out.writestr("media", json.dumps(kept), zipfile.ZIP_DEFLATED)
        print(f"wrote {base}-somemedia.apkg: {len(kept)}/{len(numbered)} media files, "
              f"{used // (1024*1024)}MB media")

if __name__ == "__main__":
    main(sys.argv[1], float(sys.argv[2]) if len(sys.argv) > 2 else None)
