//! Notes: the editable unit behind sibling cards. A note stores the
//! editor's source fields; its cards are (re)generated from them by
//! `crate::notes::generate_cards` and written here in one transaction,
//! keyed on `ord` so an edit updates existing cards in place (scheduling
//! state survives), inserts newly appearing slots, and soft-deletes slots
//! that vanished.

use std::collections::BTreeMap;

use flash_core::{CardId, DeckId, NoteId, UserId};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::notes::{GeneratedCard, NoteType};
use crate::richtext::{self, SanitizedHtml};
use crate::{Result, Store, StoreError};

use super::cards::{attach_tag, check_tags, require_deck};

#[derive(Debug, Clone, PartialEq)]
pub struct NoteRow {
    pub id: NoteId,
    pub deck_id: DeckId,
    pub note_type: NoteType,
    pub front_html: String,
    pub back_html: String,
    /// Tags of the note's cards (every card carries the same set).
    pub tags: Vec<String>,
    /// Live cards, ascending by ord.
    pub cards: Vec<(u32, CardId)>,
}

/// Everything a note save needs, beyond the generated cards.
pub struct NoteSave<'a> {
    pub note_type: NoteType,
    pub front_html: &'a SanitizedHtml,
    pub back_html: &'a SanitizedHtml,
    pub generated: &'a [GeneratedCard],
    pub tags: &'a [String],
    /// Free-plan card cap; the check runs inside the write transaction.
    pub cap: Option<u32>,
    pub now_ms: i64,
}

impl Store {
    /// Creates a note in `deck` and its cards. Rejected atomically at the
    /// cap, like `create_cards`.
    pub fn create_note(
        &self,
        user: UserId,
        deck: DeckId,
        save: &NoteSave<'_>,
    ) -> Result<(NoteId, Vec<CardId>)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        require_deck(&tx, user, deck)?;
        tx.execute(
            "INSERT INTO notes (user_id, deck_id, note_type, front_html, back_html, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                user.raw(),
                deck.0,
                save.note_type.as_str(),
                save.front_html,
                save.back_html,
                save.now_ms
            ],
        )?;
        let note = NoteId(tx.last_insert_rowid());
        let ids = replace_note_cards(&tx, user, note, deck, save)?;
        tx.commit()?;
        Ok((note, ids))
    }

    /// Rewrites a note's source and regenerates its cards in place.
    pub fn update_note(
        &self,
        user: UserId,
        note: NoteId,
        save: &NoteSave<'_>,
    ) -> Result<Vec<CardId>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let deck: Option<i64> = tx
            .query_row(
                "SELECT deck_id FROM notes WHERE id = ?1 AND user_id = ?2 AND deleted_at IS NULL",
                params![note.0, user.raw()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(deck) = deck else {
            return Err(StoreError::NotFound("note"));
        };
        tx.execute(
            "UPDATE notes SET note_type = ?1, front_html = ?2, back_html = ?3, updated_at = ?4
             WHERE id = ?5 AND user_id = ?6",
            params![
                save.note_type.as_str(),
                save.front_html,
                save.back_html,
                save.now_ms,
                note.0,
                user.raw(),
            ],
        )?;
        let ids = replace_note_cards(&tx, user, note, DeckId(deck), save)?;
        tx.commit()?;
        Ok(ids)
    }

    pub fn get_note(&self, user: UserId, note: NoteId) -> Result<Option<NoteRow>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT id, deck_id, note_type, front_html, back_html
                 FROM notes WHERE id = ?1 AND user_id = ?2 AND deleted_at IS NULL",
                params![note.0, user.raw()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, deck_id, kind, front_html, back_html)) = row else {
            return Ok(None);
        };
        let note_type = NoteType::parse(&kind).ok_or(StoreError::NotFound("note type"))?;
        let mut stmt = conn.prepare(
            "SELECT ord, id FROM cards
             WHERE note_id = ?1 AND user_id = ?2 AND deleted_at IS NULL ORDER BY ord",
        )?;
        let cards: Vec<(u32, CardId)> = stmt
            .query_map(params![id, user.raw()], |r| {
                Ok((r.get::<_, i64>(0)? as u32, CardId(r.get(1)?)))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let tags = match cards.first() {
            Some((_, card)) => super::cards::card_tags(&conn, user, *card)?,
            None => Vec::new(),
        };
        Ok(Some(NoteRow {
            id: NoteId(id),
            deck_id: DeckId(deck_id),
            note_type,
            front_html,
            back_html,
            tags,
            cards,
        }))
    }

    /// The note a live card belongs to, if any.
    pub fn note_for_card(&self, user: UserId, card: CardId) -> Result<Option<NoteRow>> {
        let note: Option<i64> = self
            .conn()
            .query_row(
                "SELECT note_id FROM cards WHERE id = ?1 AND user_id = ?2 AND deleted_at IS NULL",
                params![card.0, user.raw()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match note {
            Some(id) => self.get_note(user, NoteId(id)),
            None => Ok(None),
        }
    }

    /// Promotes a note-less card into a fresh note, so the editor can
    /// regenerate it. An imported cloze card brings its siblings along
    /// (same deck, same cloze source), each at its own index slot, so the
    /// first edit of an imported cloze note doesn't spawn duplicates.
    pub fn adopt_card_into_note(
        &self,
        user: UserId,
        card: CardId,
        note_type: NoteType,
        front_html: &SanitizedHtml,
        back_html: &SanitizedHtml,
        now_ms: i64,
    ) -> Result<NoteId> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        type SeedRow = (i64, Option<i64>, Option<String>, Option<i64>);
        let row: Option<SeedRow> = tx
            .query_row(
                "SELECT deck_id, note_id, cloze_text, cloze_index FROM cards
                 WHERE id = ?1 AND user_id = ?2 AND deleted_at IS NULL",
                params![card.0, user.raw()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((deck, note_id, cloze_text, cloze_index)) = row else {
            return Err(StoreError::NotFound("card"));
        };
        if let Some(existing) = note_id {
            return Ok(NoteId(existing));
        }
        tx.execute(
            "INSERT INTO notes (user_id, deck_id, note_type, front_html, back_html, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                user.raw(),
                deck,
                note_type.as_str(),
                front_html,
                back_html,
                now_ms
            ],
        )?;
        let note = tx.last_insert_rowid();
        match (cloze_text, cloze_index) {
            (Some(source), Some(_)) => {
                // Siblings adopt too, at ord = index - 1. Duplicate indices
                // (a re-import) keep only the lowest card id per slot.
                tx.execute(
                    "UPDATE cards SET note_id = ?1, ord = cloze_index - 1
                     WHERE id IN (
                       SELECT MIN(id) FROM cards
                       WHERE user_id = ?2 AND deck_id = ?3 AND deleted_at IS NULL
                         AND note_id IS NULL AND cloze_text = ?4 AND cloze_index IS NOT NULL
                       GROUP BY cloze_index)",
                    params![note, user.raw(), deck, source],
                )?;
            }
            _ => {
                tx.execute(
                    "UPDATE cards SET note_id = ?1, ord = 0 WHERE id = ?2 AND user_id = ?3",
                    params![note, card.0, user.raw()],
                )?;
            }
        }
        tx.commit()?;
        Ok(NoteId(note))
    }
}

/// Writes the generated cards for `note` inside `tx`; see module doc.
fn replace_note_cards(
    tx: &Transaction<'_>,
    user: UserId,
    note: NoteId,
    deck: DeckId,
    save: &NoteSave<'_>,
) -> Result<Vec<CardId>> {
    let existing: BTreeMap<u32, i64> = {
        let mut stmt = tx.prepare(
            "SELECT ord, id FROM cards WHERE note_id = ?1 AND user_id = ?2 AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![note.0, user.raw()], |r| {
            Ok((r.get::<_, i64>(0)? as u32, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let wanted: Vec<u32> = save.generated.iter().map(|g| g.ord).collect();

    // Slots that vanished go first, so their rows free the cap and the
    // unique (note_id, ord) index before anything is inserted.
    for (ord, id) in &existing {
        if !wanted.contains(ord) {
            tx.execute(
                "UPDATE cards SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND user_id = ?3",
                params![save.now_ms, id, user.raw()],
            )?;
        }
    }
    let inserts = wanted.iter().filter(|o| !existing.contains_key(o)).count() as u32;
    if let Some(cap) = save.cap {
        if inserts > 0 {
            let current: i64 = tx.query_row(
                "SELECT COUNT(*) FROM cards WHERE user_id = ?1 AND deleted_at IS NULL",
                [user.raw()],
                |r| r.get(0),
            )?;
            let current = current as u32;
            if current.saturating_add(inserts) > cap {
                return Err(StoreError::CapExceeded { current, cap });
            }
        }
    }

    let mut ids = Vec::with_capacity(save.generated.len());
    for g in save.generated {
        let x = &g.extras;
        let card_id = match existing.get(&g.ord) {
            Some(id) => {
                tx.execute(
                    "UPDATE cards SET front = ?1, back = ?2, front_html = ?3, back_html = ?4,
                       cloze_text = ?5, cloze_index = ?6, type_answer = ?7, updated_at = ?8
                     WHERE id = ?9 AND user_id = ?10",
                    params![
                        g.text.front,
                        g.text.back,
                        x.front_html,
                        x.back_html,
                        x.cloze_text,
                        x.cloze_index,
                        x.type_answer,
                        save.now_ms,
                        id,
                        user.raw()
                    ],
                )?;
                *id
            }
            None => {
                tx.execute(
                    "INSERT INTO cards (user_id, deck_id, note_id, ord, front, back,
                       front_html, back_html, cloze_text, cloze_index, type_answer,
                       created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
                    params![
                        user.raw(),
                        deck.0,
                        note.0,
                        g.ord,
                        g.text.front,
                        g.text.back,
                        x.front_html,
                        x.back_html,
                        x.cloze_text,
                        x.cloze_index,
                        x.type_answer,
                        save.now_ms
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO card_state (card_id, due)
                     SELECT ?1, ?2 FROM cards WHERE id = ?1 AND user_id = ?3",
                    params![id, save.now_ms, user.raw()],
                )?;
                id
            }
        };
        let card = CardId(card_id);
        // Tags and media links are replaced wholesale per card.
        tx.execute(
            "DELETE FROM card_tags WHERE card_id = ?1
               AND EXISTS (SELECT 1 FROM cards WHERE id = ?1 AND user_id = ?2)",
            params![card_id, user.raw()],
        )?;
        check_tags(save.tags)?;
        for tag in save.tags {
            attach_tag(tx, user, card, tag)?;
        }
        tx.execute(
            "DELETE FROM card_media WHERE card_id = ?1
               AND EXISTS (SELECT 1 FROM cards WHERE id = ?1 AND user_id = ?2)",
            params![card_id, user.raw()],
        )?;
        let mut media = richtext::media_ids(x.front_html.as_deref().unwrap_or(""));
        for id in richtext::media_ids(x.back_html.as_deref().unwrap_or("")) {
            if !media.contains(&id) {
                media.push(id);
            }
        }
        for media_id in media {
            // Ownership-guarded: a foreign id simply doesn't link (the
            // service already refused it with a proper error).
            tx.execute(
                "INSERT OR IGNORE INTO card_media (card_id, media_id)
                 SELECT ?1, m.id FROM media m WHERE m.id = ?2 AND m.user_id = ?3",
                params![card_id, media_id, user.raw()],
            )?;
        }
        ids.push(card);
    }
    Ok(ids)
}
