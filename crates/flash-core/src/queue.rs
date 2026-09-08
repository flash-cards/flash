//! Study-queue ordering: which card comes next, given what's due.
//!
//! Pure functions over slices so the policy is trivially testable. The
//! store fetches candidates; this module decides order and new-card limits.

use std::collections::HashMap;

use crate::ids::{CardId, DeckId};
use crate::rating::Phase;

/// What a study session covers.
#[derive(Debug, Clone, PartialEq)]
pub enum StudyScope {
    All,
    Deck(DeckId),
    Tag(String),
}

impl StudyScope {
    /// Serialized form stored on study_sessions.scope (e.g. "deck:3").
    pub fn as_string(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Deck(id) => format!("deck:{id}"),
            Self::Tag(tag) => format!("tag:{tag}"),
        }
    }
}

/// Minimal projection of card_state needed for queue decisions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueEntry {
    pub card_id: CardId,
    pub deck_id: DeckId,
    pub phase: Phase,
    pub due_ms: i64,
}

/// What one deck may still introduce/review today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeckBudget {
    pub new: u32,
    pub reviews: u32,
}

/// Today's remaining allowances: account-wide totals plus per-deck
/// remainders (Anki-style: each deck's own limit applies, and the account
/// limit caps the sum). A deck missing from `per_deck` is bounded only by
/// the totals.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Budgets {
    pub new_total: u32,
    pub review_total: u32,
    pub per_deck: HashMap<DeckId, DeckBudget>,
}

impl Budgets {
    /// Totals only, no per-deck limits.
    pub fn flat(new_total: u32, review_total: u32) -> Self {
        Self {
            new_total,
            review_total,
            per_deck: HashMap::new(),
        }
    }

    /// Takes one unit of `kind` for `deck` if both the deck's remainder and
    /// the total allow it.
    fn take(&mut self, deck: DeckId, new: bool) -> bool {
        let total = if new {
            &mut self.new_total
        } else {
            &mut self.review_total
        };
        if *total == 0 {
            return false;
        }
        if let Some(d) = self.per_deck.get_mut(&deck) {
            let slot = if new { &mut d.new } else { &mut d.reviews };
            if *slot == 0 {
                return false;
            }
            *slot -= 1;
        }
        *total -= 1;
        true
    }
}

/// When nothing else is due, learning cards within this window are shown
/// early rather than ending the session mid-acquisition (Anki's
/// "learn ahead limit"; covers the 10/15-minute steps).
pub const LEARN_AHEAD_MS: i64 = 20 * 60_000;

/// Orders due cards for a session:
/// 1. Learning/relearning cards whose step timer expired (most urgent —
///    they're mid-acquisition and short-interval; never capped).
/// 2. Due review cards, oldest due first (most overdue = most at risk),
///    under the per-deck and account review budgets.
/// 3. New cards in creation (id) order, under the per-deck and account
///    new-card budgets.
///
/// If that yields nothing, learning cards due within LEARN_AHEAD_MS are
/// returned instead (soonest first) so a session never strands a card in
/// a sub-day step.
pub fn order_queue(entries: &[QueueEntry], now_ms: i64, budgets: &Budgets) -> Vec<CardId> {
    let mut budgets = budgets.clone();
    let mut learning: Vec<&QueueEntry> = Vec::new();
    let mut review: Vec<&QueueEntry> = Vec::new();
    let mut new: Vec<&QueueEntry> = Vec::new();
    for e in entries {
        if e.phase != Phase::New && e.due_ms > now_ms {
            continue;
        }
        match e.phase {
            Phase::Learning | Phase::Relearning => learning.push(e),
            Phase::Review => review.push(e),
            Phase::New => new.push(e),
        }
    }
    learning.sort_by_key(|e| (e.due_ms, e.card_id));
    review.sort_by_key(|e| (e.due_ms, e.card_id));
    new.sort_by_key(|e| e.card_id);
    let review: Vec<&QueueEntry> = review
        .into_iter()
        .filter(|e| budgets.take(e.deck_id, false))
        .collect();
    let new: Vec<&QueueEntry> = new
        .into_iter()
        .filter(|e| budgets.take(e.deck_id, true))
        .collect();

    let ordered: Vec<CardId> = learning
        .into_iter()
        .chain(review)
        .chain(new)
        .map(|e| e.card_id)
        .collect();
    if !ordered.is_empty() {
        return ordered;
    }

    // Learn-ahead fallback.
    let mut ahead: Vec<&QueueEntry> = entries
        .iter()
        .filter(|e| {
            matches!(e.phase, Phase::Learning | Phase::Relearning)
                && e.due_ms <= now_ms + LEARN_AHEAD_MS
        })
        .collect();
    ahead.sort_by_key(|e| (e.due_ms, e.card_id));
    ahead.into_iter().map(|e| e.card_id).collect()
}
