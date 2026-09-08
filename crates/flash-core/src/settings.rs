//! Per-user study settings.

/// How the MCP voice flow communicates grades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GradingMode {
    /// Model judges and submits without mentioning grades or scheduling.
    #[default]
    Silent,
    /// Model states the grade in one word; user may override.
    Announce,
    /// User is asked to rate themselves every card.
    SelfGrade,
}

impl GradingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::Announce => "announce",
            Self::SelfGrade => "self",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "silent" => Some(Self::Silent),
            "announce" => Some(Self::Announce),
            "self" => Some(Self::SelfGrade),
            _ => None,
        }
    }
}

/// Hard ceilings for user-settable daily limits (Anki allows similar).
pub const MAX_NEW_PER_DAY: u32 = 9_999;
pub const MAX_REVIEWS_PER_DAY: u32 = 99_999;
pub const MAX_BOOST: u32 = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct UserSettings {
    pub grading_mode: GradingMode,
    pub desired_retention: f32,
    /// Account-wide daily new-card limit; decks may override it.
    /// (Defaults here mirror the user_settings column defaults.)
    pub new_per_day: u32,
    /// Account-wide daily review cap; decks may override it.
    pub reviews_per_day: u32,
    /// "More new cards today": extra new cards granted for the study day
    /// starting at `boost_day` (ms); ignored on any other day.
    pub boost_new: u32,
    pub boost_day: i64,
    /// Local hour at which the study "day" rolls over (e.g. 4 = 4am).
    pub day_cutoff_hour: u8,
    pub timezone: String,
    /// FSRS parameters; None means the crate defaults.
    pub fsrs_params: Option<Vec<f32>>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            grading_mode: GradingMode::Silent,
            desired_retention: 0.9,
            new_per_day: 20,
            reviews_per_day: 200,
            boost_new: 0,
            boost_day: 0,
            day_cutoff_hour: 4,
            timezone: "UTC".to_string(),
            fsrs_params: None,
        }
    }
}
