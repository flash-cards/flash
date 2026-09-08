//! Review ratings and card lifecycle phases, matching FSRS's four-grade model.

/// How well the user recalled a card. Stored in review_log as 1-4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rating {
    Again = 1,
    Hard = 2,
    Good = 3,
    Easy = 4,
}

impl Rating {
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            1 => Some(Self::Again),
            2 => Some(Self::Hard),
            3 => Some(Self::Good),
            4 => Some(Self::Easy),
            _ => None,
        }
    }

    pub fn as_i64(self) -> i64 {
        self as i64
    }
}

/// Card lifecycle phase. Stored in card_state as 0-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    New = 0,
    Learning = 1,
    Review = 2,
    Relearning = 3,
}

impl Phase {
    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            0 => Some(Self::New),
            1 => Some(Self::Learning),
            2 => Some(Self::Review),
            3 => Some(Self::Relearning),
            _ => None,
        }
    }

    pub fn as_i64(self) -> i64 {
        self as i64
    }
}
