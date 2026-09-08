# Deck-testing toolkit

Python tools (need `pip install zstandard`) for live-testing the Anki
import pipeline against real and hostile decks. All take and produce
standard `.apkg` files.

- **apkg_tool.py `<src.apkg>` `[media_budget_mb]`** — validates structure
  (entry inventory, magic-byte check on every media file: no executables,
  no scripts) and writes `<src>-nomedia.apkg` (collection only) plus,
  with a budget, `<src>-somemedia.apkg` (media kept until the budget
  fills, manifest rewritten).
- **apkg_subset.py `<src.apkg>` `<out.apkg>` `<media_mb>` `<note_cap>`** —
  cuts a fully-loaded subset: the first N notes that reference media,
  with ALL of their media, everything else deleted and the collection
  vacuumed.
- **torture_deck.py `<out.apkg>`** — builds a modern-format (.anki21b,
  zstd + protobuf) torture deck covering every import feature: cloze
  (hints/nested/same-index/12-index/malformed), {{type:}}, MathJax +
  legacy LaTeX, real audio/image media (lifted from any large deck you
  point it at), suspended/buried cards, realistic revlog with junk
  entries, FSRS-6 deck options, reversed pairs, XSS probes,
  Unicode/RTL/emoji, oversized fields.

Use decks you have the right to use; none are committed here. Built
artifacts land in `target/deck-tests/` (gitignored).
