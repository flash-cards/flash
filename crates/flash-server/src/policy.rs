//! The one place a plan can say no: whether `adding` cards may be added
//! and how large today's new-card budget may be. `Services` consults a
//! `CardPolicy` and never looks at plan state itself, so the open server
//! runs unlimited and a hosted build plugs in its cap.

use flash_core::UserId;
use flash_store::{Store, StoreError};

/// Where an add originates. A policy may treat a one-off import more
/// generously than an interactive add (web form, MCP tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSource {
    Interactive,
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Go ahead. `recheck_cap` is the live-card cap the store re-checks
    /// inside its own insert transaction (None = unlimited).
    Allowed { recheck_cap: Option<u32> },
    /// Refused: the account already holds `current` of `cap` cards.
    Denied { current: u32, cap: u32 },
}

pub trait CardPolicy: Send + Sync + 'static {
    /// Decides an add of `adding` cards. `adding == 0` never denies — an
    /// edit that inserts nothing must always go through — it only reports
    /// the cap to re-check.
    fn admit(
        &self,
        store: &Store,
        user: UserId,
        adding: u32,
        source: AddSource,
        now_ms: i64,
    ) -> Result<Admission, StoreError>;

    /// Clamps today's account-wide new-card budget.
    fn new_budget(
        &self,
        store: &Store,
        user: UserId,
        budget: u32,
        now_ms: i64,
    ) -> Result<u32, StoreError>;

    /// The sentence a refused add carries. It reaches AI assistants
    /// verbatim over MCP, so it must state the limit plainly.
    fn over_cap_message(&self, current: u32, cap: u32) -> String;
}

/// No cap, no clamp: the open server's policy.
pub struct Unlimited;

impl CardPolicy for Unlimited {
    fn admit(
        &self,
        _store: &Store,
        _user: UserId,
        _adding: u32,
        _source: AddSource,
        _now_ms: i64,
    ) -> Result<Admission, StoreError> {
        Ok(Admission::Allowed { recheck_cap: None })
    }

    fn new_budget(
        &self,
        _store: &Store,
        _user: UserId,
        budget: u32,
        _now_ms: i64,
    ) -> Result<u32, StoreError> {
        Ok(budget)
    }

    fn over_cap_message(&self, current: u32, cap: u32) -> String {
        format!("card limit reached: {current} of {cap} cards")
    }
}
