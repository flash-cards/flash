# SPDX-License-Identifier: AGPL-3.0-only
"""Build a torture-test .apkg in Anki's MODERN export format, to real spec:
schema-18 collection (notetypes/fields/templates/decks/deck_config tables
with protobuf configs), zstd-compressed collection.anki21b, protobuf
MediaEntries manifest, zstd-compressed media files, meta=version 3.

Content covers every import feature: cloze (hints, nested, multi-index),
type-in-the-answer, MathJax + legacy LaTeX, real audio + image media,
suspended & buried cards, realistic revlog history, FSRS deck options,
basic-and-reversed, rich HTML structure, nested subdecks, tags."""
import hashlib, json, random, sqlite3, struct, sys, time, zipfile, os, tempfile
import zstandard

# Two real decks to lift one image and one audio clip from — any .apkg
# you have the right to use. Point the env vars at them.
SRC_IMAGE_DECK = os.path.expanduser(os.environ.get("TORTURE_IMAGE_DECK", "~/Downloads/image-deck.apkg"))
SRC_AUDIO_DECK = os.path.expanduser(os.environ.get("TORTURE_AUDIO_DECK", "~/Downloads/audio-deck.apkg"))
OUT = sys.argv[1]

# ---- protobuf encoding helpers ----
def varint(v):
    out = b""
    while True:
        b7 = v & 0x7F
        v >>= 7
        if v:
            out += bytes([b7 | 0x80])
        else:
            return out + bytes([b7])

def tag(field, wire):
    return varint((field << 3) | wire)

def pstr(field, s):
    b = s.encode()
    return tag(field, 2) + varint(len(b)) + b

def pbytes(field, b):
    return tag(field, 2) + varint(len(b)) + b

def pvarint(field, v):
    return tag(field, 0) + varint(v)

def pfloat(field, f):
    return tag(field, 5) + struct.pack("<f", f)

# ---- real media, lifted from the real decks ----
def first_media(src, want_ext):
    z = zipfile.ZipFile(src)
    manifest = json.loads(z.read("media").decode())
    for num, name in sorted(manifest.items(), key=lambda kv: int(kv[0])):
        if name.lower().endswith(want_ext):
            data = z.read(num)
            if len(data) > 4000:          # skip tiny icons
                return data
    raise SystemExit(f"no {want_ext} in {src}")

IMG = first_media(SRC_IMAGE_DECK, ".jpg")     # a real illustration
MP3 = first_media(SRC_AUDIO_DECK, ".mp3")     # a real audio clip

# ---- notetypes ----
NT_BASIC, NT_REVERSED, NT_TYPE, NT_CLOZE, NT_VOCAB, NT_HINT = (
    1698000000001, 1698000000002, 1698000000003, 1698000000004, 1698000000005,
    1698000000006,
)
# Includes simple class color rules (Flash maps them to its palette).
CSS = (".card { font-family: arial; font-size: 20px; text-align: center; }\n"
       ".imp { color: hotpink }\n.keyword { background-color: #fff3a3 }")

def nt_config(kind):
    out = b""
    if kind:
        out += pvarint(1, kind)
    out += pstr(3, CSS)
    return out

def tmpl_config(qfmt, afmt):
    return pstr(1, qfmt) + pstr(2, afmt)

NOTETYPES = {
    NT_BASIC: ("Basic", 0, ["Front", "Back"],
               [("Card 1", "{{Front}}", "{{FrontSide}}<hr id=answer>{{Back}}")]),
    NT_REVERSED: ("Basic (and reversed card)", 0, ["Front", "Back"],
                  [("Card 1", "{{Front}}", "{{FrontSide}}<hr id=answer>{{Back}}"),
                   ("Card 2", "{{Back}}", "{{FrontSide}}<hr id=answer>{{Front}}")]),
    NT_TYPE: ("Basic (type in the answer)", 0, ["Front", "Back"],
              [("Card 1", "{{Front}}\n\n{{type:Back}}",
                "{{Front}}\n\n<hr id=answer>\n\n{{type:Back}}")]),
    NT_CLOZE: ("Cloze", 1, ["Text", "Back Extra"],
               [("Cloze", "{{cloze:Text}}", "{{cloze:Text}}<br>{{Back Extra}}")]),
    # Anki's {{hint:Field}} filter: a collapsed "show Extra" reveal.
    NT_HINT: ("Basic (hint)", 0, ["Front", "Back", "Extra"],
              [("Card 1", "{{Front}}",
                "{{FrontSide}}<hr id=answer>{{Back}}<br>{{hint:Extra}}")]),
    # Five fields, filters, conditionals, template specials — and a second
    # template whose front renders empty (conditional on a blank field).
    NT_VOCAB: ("Vocab (advanced)", 0, ["Word", "Reading", "Meaning", "Example", "Missing"],
               [("Recognition",
                 "{{Word}} ({{furigana:Reading}}) {{tts ja_JP voices=Any:Word}}",
                 "{{FrontSide}}<hr id=answer>{{Meaning}}"
                 "{{#Example}}<br><i>{{Example}}</i>{{/Example}}"
                 "{{^Example}}<br>(no example){{/Example}}<br>{{Tags}} {{Deck}}"),
                ("Ghost",
                 "{{#Missing}}{{Missing}}{{/Missing}}",
                 "{{Word}}")]),
}

# ---- decks (modern: name components joined with \x1f) ----
DECK_CARDIO, DECK_MURMURS, DECK_EMOJI = (
    1698000000101, 1698000000102, 1698000000103,
)
DECKS = {1: "Default", DECK_CARDIO: "Cardiology",
         DECK_MURMURS: "Cardiology\x1fMurmurs",
         DECK_EMOJI: "Cardiology\x1f🫀 High-Yield"}

# ---- FSRS-6 deck preset ----
FSRS6 = [0.212, 1.2931, 2.3065, 8.2956, 6.4133, 0.8334, 3.0194, 0.001,
         1.8722, 0.1666, 0.796, 1.4835, 0.0614, 0.2629, 1.6483, 0.6014,
         1.8729, 0.5425, 0.0912, 0.0658, 0.1542]

def deck_config_blob():
    packed = b"".join(struct.pack("<f", p) for p in FSRS6)
    return (pvarint(9, 15)                    # new_per_day
            + pbytes(6, packed)               # fsrs_params_6
            + pfloat(37, 0.87))               # desired_retention

# ---- notes & cards ----
NOW_MS = int(time.time() * 1000)
NOW_S = NOW_MS // 1000
CRT = NOW_S - 60 * 86400                      # collection created 60 days ago
DAY = 86400000

def guid():
    return "".join(random.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(10))

def csum(first_field):
    import re
    text = re.sub(r"<[^>]+>", "", first_field)
    return int(hashlib.sha1(text.encode()).hexdigest()[:8], 16)

# (mid, [fields], tags, [(ord, deck, queue, revlog: [(days_ago, ease, kind)])])
NOTES = [
    (NT_CLOZE,
     ["The {{c1::sinoatrial node::structure}} is the heart's primary pacemaker, "
      "firing intrinsically at {{c2::60–100}} bpm.",
      "<b>Conduction path:</b><ul><li>SA node</li><li>AV node</li>"
      "<li>Bundle of His</li><li>Purkinje fibers</li></ul><img src=\"conduction.jpg\">"],
     " cardio physiology ",
     # History includes junk a real collection can carry: an invalid ease
     # (9), a cram review (type 3), and a manual reschedule (type 4) —
     # all must be skipped by the replay, keeping the five real reviews.
     [(0, DECK_CARDIO, 2, [(40, 1, 0), (40, 3, 0), (35, 9, 1), (34, 2, 3),
                           (33, 3, 1), (20, 4, 1), (10, 0, 4), (6, 3, 1)]),
      (1, DECK_CARDIO, 0, [])]),
    (NT_CLOZE,
     ["{{c1::Beta blockers::drug class}} reduce mortality in "
      "{{c2::heart failure {{c3::with reduced ejection fraction}}}}.",
      ""],
     " cardio pharm ",
     [(0, DECK_CARDIO, 0, []), (1, DECK_CARDIO, 0, []),
      (2, DECK_CARDIO, -1, [])]),                       # suspended in Anki
    (NT_CLOZE,
     ["Mean arterial pressure: \\(MAP = DBP + \\tfrac{1}{3}(SBP - DBP)\\). "
      "Organ perfusion is threatened below {{c1::65 mmHg}}.",
      "Displayed form: \\[MAP \\approx \\frac{2\\,DBP + SBP}{3}\\]"],
     " cardio physiology math ",
     [(0, DECK_CARDIO, 0, [])]),
    (NT_BASIC,
     ["Auscultation at the apex: [sound:murmur.mp3]<br>What murmur is this?",
      "Holosystolic murmur of <b>mitral regurgitation</b>, radiating to the axilla."],
     " cardio murmurs ",
     [(0, DECK_MURMURS, 2, [(15, 3, 0), (7, 3, 1)])]),
    (NT_BASIC,
     ["Euler's identity (legacy LaTeX render check)",
      "[$$]e^{i\\pi} + 1 = 0[/$$]"],
     " math ",
     [(0, DECK_CARDIO, 0, [])]),
    (NT_REVERSED,
     ["Furosemide",
      "Loop diuretic — inhibits NKCC2 in the thick ascending limb"],
     " pharm renal ",
     [(0, DECK_CARDIO, 0, []), (1, DECK_CARDIO, -2, [])]),   # reverse buried
    (NT_TYPE,
     ["First-line reversal agent for warfarin over-anticoagulation?",
      "Vitamin K"],
     " pharm heme ",
     [(0, DECK_CARDIO, 0, [])]),
    (NT_BASIC,
     ["Adverse effects of <u>loop diuretics</u>? (OHH DANG)",
      "<ul><li><b>O</b>totoxicity</li><li><b>H</b>ypokalemia</li>"
      "<li><b>H</b>ypomagnesemia</li><li><b>D</b>ehydration</li>"
      "<li><b>A</b>llergy (sulfa)</li><li><b>N</b>ephritis</li>"
      "<li><b>G</b>out</li></ul>"],
     " pharm renal ",
     [(0, DECK_CARDIO, 0, [])]),
    # ---- hostile / edge content ----
    # XSS probe: scripts, handlers, javascript: urls, iframes — all must die.
    (NT_BASIC,
     ["XSS probe <script>alert('front')</script><img src=x onerror=\"alert(1)\"> "
      "— visible text survives",
      "<div style=\"background:url(javascript:alert(2))\" onclick=\"evil()\">"
      "styled div</div><iframe src=\"https://evil.example\"></iframe>"
      "<a href=\"javascript:alert(3)\">link</a> safe tail"],
     " security ",
     [(0, DECK_CARDIO, 0, [])]),
    # Unicode hell: CJK, RTL Arabic, Cyrillic, emoji, combining marks,
    # fullwidth chars — in the emoji-named subdeck.
    (NT_BASIC,
     ["心筋梗塞 — الاحتشاء القلبي الحاد — инфаркт миокарда 🫀",
      "étude naïve — ｚｅｎｋａｋｕ — ‏עברית‏ — é combining"],
     " unicode 国際化 ",
     [(0, DECK_EMOJI, 0, [])]),
    # Cloze edge pack: same index twice, hint with extra colons, lone c4,
    # and an unterminated c2 that must stay literal text.
    (NT_CLOZE,
     ["{{c1::First}} answer, then {{c1::second}} same-index. A lone "
      "{{c4::fourth::multi::colon::hint}} and broken {{c2::unterminated",
      "Edge-case extra"],
     " cloze edge ",
     [(0, DECK_CARDIO, 0, []), (3, DECK_CARDIO, 0, [])]),
    # Cloze markup living in the SECOND field (field-swap detection).
    (NT_CLOZE,
     ["Context sentence lives here, not the cloze.",
      "Swapped: the cloze is in field two — {{c1::detected anyway}}."],
     " cloze swap ",
     [(0, DECK_CARDIO, 0, [])]),
    # Twelve indices on one note -> twelve cards.
    (NT_CLOZE,
     ["Cranial nerve order: " + " ".join(
         "{{c%d::%s}}" % (i + 1, n) for i, n in enumerate(
             ["olfactory", "optic", "oculomotor", "trochlear", "trigeminal",
              "abducens", "facial", "vestibulocochlear", "glossopharyngeal",
              "vagus", "accessory", "hypoglossal"])),
      ""],
     " anatomy neuro ",
     [(i, DECK_CARDIO, 0, []) for i in range(12)]),
    # Math inside a cloze answer + entities (&nbsp; &amp;) in text.
    (NT_CLOZE,
     ["Ohm&nbsp;&amp;&nbsp;flow: cardiac output obeys {{c1::\\(Q = \\Delta P / R\\)::equation}}.",
      ""],
     " math cloze ",
     [(0, DECK_CARDIO, 0, [])]),
    # Media edge: percent-encoded [sound:] name with a space, single-quoted
    # and unquoted img src, plus a reference to a file NOT in the package.
    (NT_BASIC,
     ["S3 gallop: [sound:heart%20sounds.mp3] Compare: <img src='conduction.jpg'> "
      "and a missing one <img src=ghost.png>",
      "S3 = rapid ventricular filling (volume overload)."],
     " murmurs media ",
     [(0, DECK_MURMURS, 0, [])]),
    # Image-only front: no text at all on the question side.
    (NT_BASIC,
     ["<img src=\"conduction.jpg\">",
      "The cardiac conduction system."],
     " media imageonly ",
     [(0, DECK_MURMURS, 0, [])]),
    # 12k-char back (over the 10k side cap) with multi-byte chars near the
    # boundary — truncation must not panic or split a char.
    (NT_BASIC,
     ["Marathon field: what follows is deliberately oversized",
      ("Pathophysiologie détaillée — très long résumé clinique. " * 220)],
     " long ",
     [(0, DECK_CARDIO, 0, [])]),
    # Five-field vocab through filters/conditionals; the second template
    # renders an empty front (blank conditional field) -> card skipped.
    (NT_VOCAB,
     ["食べる", "たべる", "to eat", "寿司を食べてみたいです。", ""],
     " vocab 日本語 ",
     [(0, DECK_EMOJI, 0, []), (1, DECK_EMOJI, 0, [])]),
    # {{hint:Extra}} filter -> native collapsible.
    (NT_HINT,
     ["Rate-limiting enzyme of cholesterol synthesis?",
      "<b>HMG-CoA reductase</b>",
      "Statins inhibit it. Mnemonic: <i>HMG = How My Grades suffer</i>."],
     " pharm biochem hint ",
     [(0, DECK_CARDIO, 2, [(30, 1, 0), (29, 3, 1)])]),
    # AnKing-style hint button: onclick trigger + hidden div -> collapsible.
    (NT_BASIC,
     ["Amiodarone — most feared long-term toxicities?",
      "Class III antiarrhythmic. "
      "<a href=\"#\" onclick=\"this.style.display='none';"
      "document.getElementById('tox42').style.display='';return false;\">"
      "Show toxicities</a>"
      "<div id=\"tox42\" style=\"display:none\"><ul><li>Pulmonary fibrosis</li>"
      "<li>Thyroid dysfunction</li><li>Corneal deposits</li></ul></div>"],
     " pharm cardio jsreveal ",
     [(0, DECK_CARDIO, 2, [(30, 1, 0), (29, 3, 1)])]),
    # Color soup: font tag, inline styles (text + background), <mark>, and
    # a CSS-classed span -> all remap to Flash palette classes.
    (NT_BASIC,
     ["<font color=\"red\">RED flag</font> drug interactions "
      "(<span style=\"color: dodgerblue\">blue = CYP inhibitor</span>)",
      "<span class=\"imp\">Amiodarone</span> raises "
      "<span style=\"background-color: yellow\">digoxin</span> and "
      "<mark>warfarin</mark> levels; <span class=\"keyword\">monitor INR</span>. "
      "<span style=\"color: #808080\">gray note stays plain</span>"],
     " pharm colors ",
     [(0, DECK_CARDIO, 2, [(30, 1, 0), (29, 3, 1)])]),
]

def build_collection(path):
    conn = sqlite3.connect(path)
    conn.create_collation("unicase", lambda a, b: (a.lower() > b.lower()) - (a.lower() < b.lower()))
    conn.executescript("""
CREATE TABLE col (id integer primary key, crt integer not null, mod integer not null,
  scm integer not null, ver integer not null, dty integer not null, usn integer not null,
  ls integer not null, conf text not null, models text not null, decks text not null,
  dconf text not null, tags text not null);
CREATE TABLE notes (id integer primary key, guid text not null, mid integer not null,
  mod integer not null, usn integer not null, tags text not null, flds text not null,
  sfld integer not null, csum integer not null, flags integer not null, data text not null);
CREATE TABLE cards (id integer primary key, nid integer not null, did integer not null,
  ord integer not null, mod integer not null, usn integer not null, type integer not null,
  queue integer not null, due integer not null, ivl integer not null, factor integer not null,
  reps integer not null, lapses integer not null, left integer not null, odue integer not null,
  odid integer not null, flags integer not null, data text not null);
CREATE TABLE revlog (id integer primary key, cid integer not null, usn integer not null,
  ease integer not null, ivl integer not null, lastIvl integer not null, factor integer not null,
  time integer not null, type integer not null);
CREATE TABLE decks (id integer primary key not null, name text not null collate unicase,
  mtime_secs integer not null, usn integer not null, common blob not null, kind blob not null);
CREATE TABLE deck_config (id integer primary key not null, name text not null collate unicase,
  mtime_secs integer not null, usn integer not null, config blob not null);
CREATE TABLE notetypes (id integer not null primary key, name text not null collate unicase,
  mtime_secs integer not null, usn integer not null, config blob not null);
CREATE TABLE fields (ntid integer not null, ord integer not null,
  name text not null collate unicase, config blob not null, primary key (ntid, ord));
CREATE TABLE templates (ntid integer not null, ord integer not null,
  name text not null collate unicase, mtime_secs integer not null, usn integer not null,
  config blob not null, primary key (ntid, ord));
CREATE TABLE config (key text not null primary key, usn integer not null,
  mtime_secs integer not null, val blob not null);
CREATE TABLE graves (oid integer not null, type integer not null, usn integer not null,
  primary key (oid, type));
CREATE TABLE tags (tag text not null primary key collate unicase, usn integer not null,
  collapsed boolean not null, config blob null);
CREATE INDEX ix_notes_usn ON notes (usn);
CREATE INDEX ix_cards_usn ON cards (usn);
CREATE INDEX ix_revlog_usn ON revlog (usn);
CREATE INDEX ix_cards_nid ON cards (nid);
CREATE INDEX ix_cards_sched ON cards (did, queue, due);
CREATE INDEX ix_revlog_cid ON revlog (cid);
CREATE INDEX ix_notes_csum ON notes (csum);
""")
    conn.execute(
        "INSERT INTO col VALUES (1, ?, ?, ?, 18, 0, 0, 0, '{}', '', '', '', '')",
        (CRT, NOW_MS, NOW_MS),
    )
    for ntid, (name, kind, fields, templates) in NOTETYPES.items():
        conn.execute("INSERT INTO notetypes VALUES (?, ?, ?, -1, ?)",
                     (ntid, name, NOW_S, nt_config(kind)))
        for ord_, fname in enumerate(fields):
            conn.execute("INSERT INTO fields VALUES (?, ?, ?, ?)",
                         (ntid, ord_, fname, pstr(3, "Arial")))
        for ord_, (tname, qfmt, afmt) in enumerate(templates):
            conn.execute("INSERT INTO templates VALUES (?, ?, ?, ?, -1, ?)",
                         (ntid, ord_, tname, NOW_S, tmpl_config(qfmt, afmt)))
    kind_normal = pbytes(1, pvarint(1, 1))     # DeckKindContainer{normal{config_id:1}}
    for did, name in DECKS.items():
        conn.execute("INSERT INTO decks VALUES (?, ?, ?, -1, ?, ?)",
                     (did, name, NOW_S, b"", kind_normal))
    conn.execute("INSERT INTO deck_config VALUES (1, 'Default', ?, -1, ?)",
                 (NOW_S, deck_config_blob()))

    note_id, card_id, pos = NOW_MS, NOW_MS + 500, 0
    for mid, fields, tags_, cards in NOTES:
        flds = "\x1f".join(fields)
        conn.execute("INSERT INTO notes VALUES (?, ?, ?, ?, -1, ?, ?, ?, ?, 0, '')",
                     (note_id, guid(), mid, NOW_S, tags_, flds, fields[0], csum(fields[0])))
        for ord_, did, queue, revlog in cards:
            pos += 1
            reviewed = bool(revlog)
            ctype = 2 if reviewed else 0
            cqueue = queue if queue < 0 else (2 if reviewed else 0)
            due = ((NOW_S - CRT) // 86400 + 3) if reviewed else pos
            ivl = 14 if reviewed else 0
            conn.execute(
                "INSERT INTO cards VALUES (?, ?, ?, ?, ?, -1, ?, ?, ?, ?, 0, ?, 0, 0, 0, 0, 0, '')",
                (card_id, note_id, did, ord_, NOW_S, ctype, cqueue, due, ivl, len(revlog)))
            for days_ago, ease, kind in revlog:
                rid = NOW_MS - days_ago * DAY + random.randint(0, 3_600_000)
                conn.execute("INSERT INTO revlog VALUES (?, ?, -1, ?, 14, 7, 0, 4500, ?)",
                             (rid, card_id, ease, kind))
            card_id += 1
        note_id += 1
    conn.commit()
    conn.close()

# ---- assemble the modern package ----
tmp = os.path.join(tempfile.mkdtemp(), "collection.sqlite")
build_collection(tmp)
zc = zstandard.ZstdCompressor(level=3)

# "heart sounds.mp3" shares bytes with murmur.mp3 on purpose: two names,
# one blob — exercises content-addressed dedupe. Its space-in-name form is
# referenced percent-encoded from the field.
media_files = [("conduction.jpg", IMG), ("murmur.mp3", MP3),
               ("heart sounds.mp3", MP3)]
entries = b""
for name, data in media_files:
    entry = (pstr(1, name) + pvarint(2, len(data))
             + pbytes(3, hashlib.sha1(data).digest()))
    entries += pbytes(1, entry)

with zipfile.ZipFile(OUT, "w") as z:
    z.writestr("meta", pvarint(1, 3))                          # version 3
    with open(tmp, "rb") as f:
        z.writestr("collection.anki21b", zc.compress(f.read()))
    z.writestr("media", zc.compress(entries))
    for i, (_, data) in enumerate(media_files):
        z.writestr(str(i), zc.compress(data))

total_cards = sum(len(c) for _, _, _, c in NOTES)
print(f"torture deck: {len(NOTES)} notes, {total_cards} anki cards, "
      f"{len(media_files)} media files, {os.path.getsize(OUT)//1024}KB -> {OUT}")
