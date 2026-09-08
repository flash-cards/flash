# SPDX-License-Identifier: AGPL-3.0-only
"""Cut a fully-loaded subset out of a big .apkg: keep the first N notes
that reference media, with ALL of their media files, until a byte budget
fills. Everything else (notes, cards, media) is dropped and the SQLite
collection is vacuumed. Output is a valid legacy .apkg."""
import html, json, re, sqlite3, sys, tempfile, urllib.parse, zipfile, os

SRC, OUT = sys.argv[1], sys.argv[2]
MEDIA_BUDGET = int(float(sys.argv[3]) * 1024 * 1024)
NOTE_CAP = int(sys.argv[4])

z = zipfile.ZipFile(SRC)
manifest = json.loads(z.read("media").decode())          # num -> filename
name_to_num = {v: k for k, v in manifest.items()}

tmp = os.path.join(tempfile.mkdtemp(), "c.anki2")
with open(tmp, "wb") as f:
    f.write(z.read("collection.anki2"))
conn = sqlite3.connect(tmp)

ref_re = re.compile(r'src="([^"]+)"|\[sound:([^\]]+)\]')
selected, media_needed, total = [], {}, 0
for nid, flds in conn.execute("SELECT id, flds FROM notes ORDER BY id"):
    refs = set()
    for m in ref_re.finditer(flds):
        raw = html.unescape(m.group(1) or m.group(2))
        for cand in (raw, urllib.parse.unquote(raw)):
            if cand in name_to_num:
                refs.add(cand)
                break
    if not refs:
        continue
    new = [r for r in refs if r not in media_needed]
    add = sum(z.getinfo(name_to_num[r]).file_size for r in new)
    if total + add > MEDIA_BUDGET:
        continue
    for r in new:
        media_needed[r] = name_to_num[r]
    total += add
    selected.append(nid)
    if len(selected) >= NOTE_CAP:
        break

ids = ",".join(str(i) for i in selected)
conn.execute(f"DELETE FROM notes WHERE id NOT IN ({ids})")
conn.execute(f"DELETE FROM cards WHERE nid NOT IN ({ids})")
conn.execute("DELETE FROM revlog")
conn.commit()
conn.execute("VACUUM")
conn.close()

with zipfile.ZipFile(OUT, "w") as out:
    out.write(tmp, "collection.anki2", zipfile.ZIP_DEFLATED)
    for name, num in media_needed.items():
        out.writestr(num, z.read(num))
    out.writestr(
        "media", json.dumps({num: name for name, num in media_needed.items()})
    )
print(f"kept {len(selected)} notes, {len(media_needed)} media files "
      f"({total // (1024*1024)}MB); output {os.path.getsize(OUT) // (1024*1024)}MB")
