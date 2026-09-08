//! Replays py-fsrs (FSRS-6) reference vectors through the Scheduler wrapper
//! and asserts the memory state matches. The vectors in
//! tests/fsrs_vectors.json were produced with py-fsrs, fuzzing and learning
//! steps disabled (see THIRD_PARTY.md).

use flash_core::scheduler::MS_PER_DAY;
use flash_core::{CardState, Rating, Scheduler};

const VECTORS: &str = include_str!("fsrs_vectors.json");

/// f32 pipeline vs py-fsrs f64: allow small relative drift.
const REL_TOL: f64 = 2e-3;

fn rating(name: &str) -> Rating {
    match name {
        "again" => Rating::Again,
        "hard" => Rating::Hard,
        "good" => Rating::Good,
        "easy" => Rating::Easy,
        other => panic!("unknown rating {other}"),
    }
}

fn assert_close(what: &str, seq: &str, step: usize, got: f64, want: f64) {
    let denom = want.abs().max(1e-6);
    assert!(
        ((got - want) / denom).abs() < REL_TOL,
        "{seq} step {step}: {what} mismatch: got {got}, want {want}"
    );
}

#[test]
fn matches_py_fsrs_reference_vectors() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
    let scheduler = Scheduler::new(None, 0.9).unwrap();

    for seq in doc["sequences"].as_array().unwrap() {
        let name = seq["name"].as_str().unwrap();
        let mut state = CardState::new_card(0);
        let mut now_ms = 0i64;
        for (i, step) in seq["steps"].as_array().unwrap().iter().enumerate() {
            now_ms += step["elapsed_days"].as_i64().unwrap() * MS_PER_DAY;
            let outcome = scheduler
                .review(&state, rating(step["rating"].as_str().unwrap()), now_ms)
                .unwrap();
            assert_close(
                "stability",
                name,
                i,
                outcome.stability_after as f64,
                step["stability"].as_f64().unwrap(),
            );
            assert_close(
                "difficulty",
                name,
                i,
                outcome.difficulty_after as f64,
                step["difficulty"].as_f64().unwrap(),
            );
            state = outcome.state;
        }
    }
}

#[test]
fn due_is_never_in_the_past() {
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let ratings = [Rating::Again, Rating::Hard, Rating::Good, Rating::Easy];
    // Walk every 3-review rating sequence from a new card.
    for &r1 in &ratings {
        for &r2 in &ratings {
            for &r3 in &ratings {
                let mut state = CardState::new_card(0);
                let mut now_ms = 0i64;
                for r in [r1, r2, r3] {
                    let outcome = scheduler.review(&state, r, now_ms).unwrap();
                    assert!(
                        outcome.state.due_ms > now_ms,
                        "due not in the future after {r1:?},{r2:?},{r3:?}"
                    );
                    assert!(outcome.state.stability.unwrap() > 0.0);
                    state = outcome.state;
                    // Next review happens exactly when the card comes due.
                    now_ms = state.due_ms;
                }
            }
        }
    }
}

#[test]
fn review_lapse_increments_lapses_and_relearns() {
    let scheduler = Scheduler::new(None, 0.9).unwrap();
    let state = CardState::new_card(0);
    let graduated = scheduler.review(&state, Rating::Good, 0).unwrap().state;
    assert_eq!(graduated.phase, flash_core::Phase::Review);

    let lapsed = scheduler
        .review(&graduated, Rating::Again, graduated.due_ms)
        .unwrap()
        .state;
    assert_eq!(lapsed.phase, flash_core::Phase::Relearning);
    assert_eq!(lapsed.lapses, 1);
    // Relearning step is sub-day: back within the session, not tomorrow.
    assert!(lapsed.due_ms - graduated.due_ms < MS_PER_DAY);
}
