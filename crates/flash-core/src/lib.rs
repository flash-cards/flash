//! Pure domain logic: no IO, no async, no SQLite.

pub mod card;
pub mod ids;
pub mod queue;
pub mod rating;
pub mod scheduler;
pub mod settings;

pub use card::{validate_card_text, CardText, MAX_SIDE_LEN};
pub use ids::{CardId, DeckId, MediaId, NoteId, SessionId, UserId};
pub use rating::{Phase, Rating};
pub use scheduler::{CardState, ReviewOutcome, Scheduler, SchedulerError};
pub use settings::{GradingMode, UserSettings};
