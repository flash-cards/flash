# Changelog

Every user-visible change to the open core, newest first. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
versions follow [Semantic Versioning](https://semver.org/): before 1.0, a
minor bump may change behaviour and a patch bump only fixes. Each entry
names the issue it answers and the person who raised it.

## [Unreleased]

### Fixed

- Gemini's custom MCP connector now connects. Flash serves the
  authorization-server metadata at the path-suffixed well-known URL,
  accepts the server origin as the OAuth `resource`, accepts an OAuth
  `state` of up to 4 KB (including through the sign-in redirect), and
  answers MCP requests with plain JSON instead of an SSE stream that
  opened with a `retry:` frame Gemini could not parse. The MCP endpoint
  is now stateless: `GET /mcp` and `DELETE /mcp` answer 405 and no
  `Mcp-Session-Id` is issued, which the MCP specification allows and
  the clients we tried (Gemini, Grok, ChatGPT, Claude) accept.
  (#10, reported by @DHEERAJKUMAR24)

## [0.3.0] - 2026-09-19

Upgrading from 0.2.0 needs the new image or binary and one step for each
connected AI client: the database moves from schema 22 to 24 on the first
boot, with every deck, card, review and media file kept, and the MCP tools
changed (see "Changed" below), so refresh the connector's tool definitions
in each client after the upgrade. A deck whose name already held `::`
becomes a subdeck of the deck that name points at, when there is one,
and is then studied, archived and deleted along with it. See "Upgrading"
in the README.

### Added

- Decks nest, the way Anki's do. A deck's name is a path, so
  `Pharmacology::Cardio` is the deck `Cardio` inside `Pharmacology`; the
  deck list shows a subdeck indented under its deck, by its own name.
  Make one with "Add subdeck" on a deck's page, move a deck with "Parent
  deck" in its settings, or name a path anywhere a deck is named, which
  also creates the decks on the way. Studying a deck includes its
  subdecks, and its due and new counts hold theirs, while each card still
  spends the daily limits of the deck it lives in. Renaming, archiving
  and deleting a deck carry the decks under it, and unarchiving a subdeck
  brings back the decks it sits in. In the JSON API a deck gains `leaf`,
  `depth` and `parent_id`, a deck's detail gains `subdeck_count`,
  `POST /api/v1/decks` takes an optional `parent_id`, and
  `POST /api/v1/decks/{id}/move` moves a deck. Over MCP every tool that
  takes a deck name takes a path, `rename_deck` renames or moves a deck,
  and `delete_deck` names the subdecks it would remove before it asks.
  The database moves to schema 24 on the first boot.
- A deck that has subdecks folds in the deck list and on Today. Every
  deck starts open; one you fold shut stays shut in that browser until
  you open it again, remembered the way "Show archived" is, and its
  counts keep holding what it hides. Folding, like "Show archived",
  happens at once with no request: the page already holds every row.
  A deck's own page has a "Subdecks" link beside Settings that opens and
  closes the list of its subdecks, with how many there are; it is closed
  whenever the page loads and is not remembered, and an archived subdeck
  is unarchived from its row there, as on the deck list. "Generate" on a
  deck's page moved from the buttons in the heading to a link across
  from "Add a card". A deck's path reads `Pharmacology › Cardio`
  wherever a page shows one (a picker, a title, the import preview);
  `::` is what is stored and what the API, MCP and Anki files use.
- An import keeps the decks its file names. An Anki package's nested
  decks arrive nested, where they used to arrive by their last name
  only and merged into one deck; a CSV's `deck` column does the same.
  The preview lists the file's decks with their card counts and asks
  only for an optional deck to put them under. A deck you already have
  by that name gets the cards, and a name that cannot be made (too long,
  inside an archived deck, past the deck limit) stops the import before
  the first card. A file that names one deck, or none, imports as
  before. Over the API a client sends `keep_decks` on
  `POST /api/v1/import/commit` and reads `file_decks` from the preview;
  without the flag the decks merge as they did. An export writes the
  decks a subdeck sits in, so Anki opens the package with its tree.
- MCP card updates and deletions accept batches, with one confirmation
  for a deletion batch. Reads include pagination and total counts, and
  bound each page by size so a deck of rich cards never overflows a
  client; writes return affected IDs and counts.
- Generated cards can go into a new deck named on the spot. The deck
  switcher on the generate page and on the review of the drafts offers
  "New deck" beside the decks you have, so an account with none is no
  longer sent away to make one first. The API takes the same thing as
  `deck_name` on `POST /ai/jobs` and on a keep.

### Changed

- The MCP contract changes, with no aliases for the old tools:
  `update_cards` and `delete_cards` replace the single-card tools, and
  content uses HTML source fields and media references. Spoken fields are
  removed: clients choose how to display or speak cards. Refresh connector
  tool definitions after upgrading. See
  [MCP contract](crates/flash-server/MCP.md).
- Archived decks show in place in the deck list, with "(archived)" after
  the name, instead of in a section of their own under it. "Show
  archived" and "Hide archived" only decide whether they are listed, and
  the choice is remembered by the browser: it holds across a refresh and
  a later visit until it is turned off, and the address stays `/decks`.
  The JSON API and the MCP `list_decks` tool return decks in tree order,
  no longer with the archived ones last.
- A theme applies the moment it is picked in Settings, without a Save and
  a reload.
- A page's title now shares a centre with the buttons across from it. It
  sat a few pixels high of them on every page that has both.

### Fixed

- MCP reads expose complete card content, and edits preserve formatting
  and media references. Media URLs use the connector's existing
  credentials. Partial edits keep untouched fields and shared card
  sources intact. (#2, reported by @xblr)
- MCP deletion confirmations survive reconnects and server restarts,
  last ten minutes, and distinguish expiry from other failures. Retrying
  a completed deletion returns its original result. A deck deletion asks
  about the cards the deck shows. Pending approvals cannot
  delete replacement cards or decks that reuse an original target's ID.
- Long pages no longer scroll past their own content. The review of
  generated cards grew a second scrollbar that ran for the height of the
  card list and pulled the sidebar off the top of the window.

## [0.2.0] - 2026-09-17

Upgrading from 0.1.0 needs nothing beyond the new image or binary: the
database moves from schema 15 to 22 on the first boot, with every deck,
card, review and media file kept. See "Upgrading" in the README.

### Added

- Decks can be archived. An archived deck keeps its cards, media and
  review history but leaves the study queue until it is unarchived. On
  the web, in the JSON API (`POST /api/v1/decks/{id}/archive` and
  `/unarchive`) and as the MCP tools `archive_deck` and `unarchive_deck`.
  (#1, suggested by @xblr)
- Card generation with your own model. PDFs, images, video, audio,
  YouTube links and pasted text become drafts you review, edit by chat
  and keep. `FLASH_AI_PROVIDER` picks the API: `openai` for any endpoint
  speaking the OpenAI Chat Completions protocol, hosted or on your own
  machine, or `gemini` for the native adapter, which also reads PDFs,
  video and YouTube links. See "Using your own model" in the README.
- Voice mode: study hands-free, natively in the app and on the web. The
  cards are read to you in a natural voice from the `tools/flash-tts`
  sidecar (`FLASH_TTS_URL`, with English, Japanese and Mandarin voices),
  your spoken answer is graded by the model, and you can ask about the
  card, grade, repeat or skip by speaking.
- An Anki import brings the whole review history, and the stats page can
  show a single year.

## [0.1.0] - 2026-09-07

The open core as first published: the FSRS scheduler, the SQLite store
with Anki import and export, and the server with its web UI, JSON API and
MCP endpoint (Streamable HTTP with OAuth 2.1). Accounts are created by an
admin; the first admin enrolls from a one-time link in the boot log. Ships
as a static Linux binary and a Docker image.

[Unreleased]: https://github.com/flash-cards/flash/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/flash-cards/flash/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/flash-cards/flash/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/flash-cards/flash/releases/tag/v0.1.0
