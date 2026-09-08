use flash_core::queue::{order_queue, Budgets, DeckBudget, QueueEntry};
use flash_core::{CardId, DeckId, Phase};

fn entry(id: i64, phase: Phase, due_ms: i64) -> QueueEntry {
    entry_in(id, 1, phase, due_ms)
}

fn entry_in(id: i64, deck: i64, phase: Phase, due_ms: i64) -> QueueEntry {
    QueueEntry {
        card_id: CardId(id),
        deck_id: DeckId(deck),
        phase,
        due_ms,
    }
}

fn flat(new: u32) -> Budgets {
    Budgets::flat(new, u32::MAX)
}

#[test]
fn learning_before_review_before_new() {
    let now = 1_000_000;
    let entries = [
        entry(1, Phase::New, 0),
        entry(2, Phase::Review, now - 500),
        entry(3, Phase::Learning, now - 100),
        entry(4, Phase::Relearning, now - 200),
    ];
    let order = order_queue(&entries, now, &flat(10));
    assert_eq!(
        order,
        vec![CardId(4), CardId(3), CardId(2), CardId(1)],
        "relearning (older due) then learning, then review, then new"
    );
}

#[test]
fn future_cards_are_excluded_and_new_budget_applies() {
    let now = 1_000_000;
    let entries = [
        entry(1, Phase::Review, now + 1),        // not yet due
        entry(2, Phase::Learning, now + 60_000), // step timer not expired
        entry(3, Phase::New, 0),
        entry(4, Phase::New, 0),
        entry(5, Phase::New, 0),
    ];
    let order = order_queue(&entries, now, &flat(2));
    assert_eq!(order, vec![CardId(3), CardId(4)]);
}

#[test]
fn learn_ahead_only_when_nothing_else_is_due() {
    let now = 1_000_000;
    let soon = now + 5 * 60_000; // learning step expires in 5 min
                                 // Something else is due -> learn-ahead card excluded.
    let entries = [entry(1, Phase::Learning, soon), entry(2, Phase::New, 0)];
    assert_eq!(order_queue(&entries, now, &flat(10)), vec![CardId(2)]);

    // Nothing else due -> the upcoming learning card is pulled forward.
    let entries = [entry(1, Phase::Learning, soon)];
    assert_eq!(order_queue(&entries, now, &flat(10)), vec![CardId(1)]);

    // But not if it's beyond the 20-minute learn-ahead window.
    let entries = [entry(1, Phase::Learning, now + 60 * 60_000)];
    assert!(order_queue(&entries, now, &flat(10)).is_empty());
}

#[test]
fn overdue_reviews_come_oldest_first() {
    let now = 1_000_000;
    let entries = [
        entry(1, Phase::Review, now - 10),
        entry(2, Phase::Review, now - 999),
        entry(3, Phase::Review, now - 500),
    ];
    let order = order_queue(&entries, now, &flat(0));
    assert_eq!(order, vec![CardId(2), CardId(3), CardId(1)]);
}

#[test]
fn per_deck_new_limits_apply_under_the_account_total() {
    let now = 1_000_000;
    // Deck 1 has 3 new, deck 2 has 3 new; deck 1 may introduce 1, deck 2
    // is unlimited per-deck, account total is 3.
    let entries = [
        entry_in(1, 1, Phase::New, 0),
        entry_in(2, 1, Phase::New, 0),
        entry_in(3, 1, Phase::New, 0),
        entry_in(4, 2, Phase::New, 0),
        entry_in(5, 2, Phase::New, 0),
        entry_in(6, 2, Phase::New, 0),
    ];
    let mut budgets = Budgets::flat(3, u32::MAX);
    budgets.per_deck.insert(
        DeckId(1),
        DeckBudget {
            new: 1,
            reviews: u32::MAX,
        },
    );
    let order = order_queue(&entries, now, &budgets);
    assert_eq!(order, vec![CardId(1), CardId(4), CardId(5)]);

    // A deck whose remainder is zero introduces nothing even with total left.
    budgets.per_deck.insert(
        DeckId(2),
        DeckBudget {
            new: 0,
            reviews: u32::MAX,
        },
    );
    let order = order_queue(&entries, now, &budgets);
    assert_eq!(order, vec![CardId(1)]);
}

#[test]
fn review_cap_limits_reviews_but_never_learning() {
    let now = 1_000_000;
    let entries = [
        entry_in(1, 1, Phase::Learning, now - 1),
        entry_in(2, 1, Phase::Relearning, now - 1),
        entry_in(3, 1, Phase::Review, now - 300),
        entry_in(4, 1, Phase::Review, now - 200),
        entry_in(5, 1, Phase::Review, now - 100),
    ];
    // Deck review cap 1: oldest review only; learning cards untouched.
    let mut budgets = Budgets::flat(0, u32::MAX);
    budgets
        .per_deck
        .insert(DeckId(1), DeckBudget { new: 0, reviews: 1 });
    assert_eq!(
        order_queue(&entries, now, &budgets),
        vec![CardId(1), CardId(2), CardId(3)]
    );
    // Account review total 2 caps across decks.
    let budgets = Budgets::flat(0, 2);
    assert_eq!(
        order_queue(&entries, now, &budgets),
        vec![CardId(1), CardId(2), CardId(3), CardId(4)]
    );
}
