//! Wire types shared across API modules, with the conversions from the
//! `Services` structs they mirror. Kept apart from those structs so the
//! JSON shape can evolve without touching domain code.

use serde::Serialize;

use crate::service::{self, AccountOverview};

/// A deck with today's budgeted counts, as the lists show it.
#[derive(Debug, Serialize)]
pub struct Deck {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub due: u32,
    pub new: u32,
}

impl From<flash_store::DeckSummary> for Deck {
    fn from(d: flash_store::DeckSummary) -> Self {
        Deck {
            id: d.id.0,
            name: d.name,
            description: d.description,
            due: d.due_count,
            new: d.new_count,
        }
    }
}

/// A card as the deck lists and the editor see it.
#[derive(Debug, Serialize)]
pub struct Card {
    pub id: i64,
    pub deck_id: i64,
    pub front: String,
    pub back: String,
    pub front_html: Option<String>,
    pub back_html: Option<String>,
    pub tags: Vec<String>,
    /// "cloze 2" / "reversed" / "typed" / null for a plain basic card.
    pub kind: Option<String>,
    pub suspended: bool,
    pub note_id: Option<i64>,
    pub ord: u32,
}

impl From<flash_store::CardRow> for Card {
    fn from(c: flash_store::CardRow) -> Self {
        Card {
            kind: service::card_kind(&c),
            id: c.id.0,
            deck_id: c.deck_id.0,
            front: c.front,
            back: c.back,
            front_html: c.front_html,
            back_html: c.back_html,
            tags: c.tags,
            suspended: c.suspended,
            note_id: c.note_id.map(|n| n.0),
            ord: c.ord,
        }
    }
}

/// A deck's daily-limit overrides beside the account defaults they
/// inherit when null, plus today's boost.
#[derive(Debug, Serialize)]
pub struct DeckLimits {
    pub new_per_day: Option<u32>,
    pub reviews_per_day: Option<u32>,
    pub default_new: u32,
    pub default_reviews: u32,
    pub boost_today: u32,
}

#[derive(Debug, Serialize)]
pub struct DeckDetail {
    pub deck: Deck,
    pub limits: DeckLimits,
    pub card_count: u32,
}

impl From<service::DeckDetail> for DeckDetail {
    fn from(d: service::DeckDetail) -> Self {
        DeckDetail {
            deck: d.deck.into(),
            limits: DeckLimits {
                new_per_day: d.limits.new_per_day,
                reviews_per_day: d.limits.reviews_per_day,
                default_new: d.default_new,
                default_reviews: d.default_reviews,
                boost_today: d.boost_today,
            },
            card_count: d.card_count,
        }
    }
}

/// What the editor opens with.
#[derive(Debug, Serialize)]
pub struct EditorSeed {
    pub card_id: i64,
    pub deck_id: i64,
    pub note_id: Option<i64>,
    pub note_type: &'static str,
    pub front_html: String,
    pub back_html: String,
    pub tags: Vec<String>,
    pub sibling_count: usize,
}

impl From<service::EditorSeed> for EditorSeed {
    fn from(s: service::EditorSeed) -> Self {
        EditorSeed {
            card_id: s.card_id.0,
            deck_id: s.deck_id.0,
            note_id: s.note_id.map(|n| n.0),
            note_type: s.note_type.as_str(),
            front_html: s.front_html,
            back_html: s.back_html,
            tags: s.tags,
            sibling_count: s.sibling_count,
        }
    }
}

/// The card a study screen shows, plus the media to warm.
#[derive(Debug, Serialize)]
pub struct StudyCard {
    pub card_id: i64,
    pub front: String,
    pub front_html: Option<String>,
    pub wants_typing: bool,
    /// Image ids on this card's hidden back: fetch before the reveal.
    pub preload_media: Vec<i64>,
    /// Image ids on the next card, both sides: fetch at low priority.
    pub prefetch_media: Vec<i64>,
}

impl From<service::StudyCard> for StudyCard {
    fn from(c: service::StudyCard) -> Self {
        StudyCard {
            card_id: c.card_id.0,
            front: c.front,
            front_html: c.front_html,
            wants_typing: c.wants_typing,
            preload_media: c.preload_media,
            prefetch_media: c.prefetch_media,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DiffSpan {
    pub text: String,
    pub ok: bool,
}

impl From<service::DiffSpan> for DiffSpan {
    fn from(s: service::DiffSpan) -> Self {
        DiffSpan {
            text: s.text,
            ok: s.ok,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Reveal {
    pub back: String,
    pub back_html: Option<String>,
    /// Present when the card asked for typing and something was typed.
    pub typed_diff: Option<Vec<DiffSpan>>,
}

impl From<service::Reveal> for Reveal {
    fn from(r: service::Reveal) -> Self {
        Reveal {
            back: r.back,
            back_html: r.back_html,
            typed_diff: r
                .typed_diff
                .map(|spans| spans.into_iter().map(Into::into).collect()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub reviewed: u32,
    pub again: u32,
    pub hard: u32,
    pub good: u32,
    pub easy: u32,
}

impl From<flash_store::SessionStats> for SessionSummary {
    fn from(s: flash_store::SessionStats) -> Self {
        SessionSummary {
            reviewed: s.reviewed,
            again: s.again,
            hard: s.hard,
            good: s.good,
            easy: s.easy,
        }
    }
}

/// The signed-in account as the app sees it. The extension's fields
/// (plan, connected providers) are flattened in through `extra`.
#[derive(Debug, Serialize)]
pub struct Me {
    pub id: i64,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub theme: String,
    pub grading_mode: &'static str,
    pub login_methods: LoginMethods,
    #[serde(flatten)]
    pub extra: crate::ext::JsonFields,
}

#[derive(Debug, Serialize)]
pub struct LoginMethods {
    pub password: bool,
    pub passkeys: u32,
}

impl From<AccountOverview> for Me {
    fn from(a: AccountOverview) -> Self {
        Me {
            id: a.id.raw(),
            display_name: a.display_name,
            email: a.email,
            role: a.role,
            theme: a.theme,
            grading_mode: a.grading_mode.as_str(),
            login_methods: LoginMethods {
                password: a.has_password,
                passkeys: a.passkeys,
            },
            extra: crate::ext::JsonFields::new(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Passkey {
    pub id: i64,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Which ways into the account exist; external providers are the
/// extension's and arrive in its `providers` field.
#[derive(Debug, Serialize)]
pub struct SettingsLoginMethods {
    pub password: bool,
    pub passkeys: Vec<Passkey>,
}

/// Everything the settings screen renders. The extension's fields (plan,
/// usage, billing, connected providers) are flattened in through `extra`.
#[derive(Debug, Serialize)]
pub struct Settings {
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub theme: String,
    pub grading_mode: &'static str,
    pub new_per_day: u32,
    pub reviews_per_day: u32,
    pub timezone: String,
    pub day_cutoff_hour: u8,
    pub desired_retention: f32,
    pub login_methods: SettingsLoginMethods,
    pub card_count: u32,
    #[serde(flatten)]
    pub extra: crate::ext::JsonFields,
}

impl Settings {
    pub fn from_overview(o: service::SettingsOverview) -> Self {
        Settings {
            login_methods: SettingsLoginMethods {
                password: o.has_password,
                passkeys: o
                    .passkeys
                    .iter()
                    .map(|p| Passkey {
                        id: p.id,
                        label: p.label.clone(),
                        created_at: p.created_at,
                        last_used_at: p.last_used_at,
                    })
                    .collect(),
            },
            display_name: o.display_name,
            email: o.email,
            role: o.role,
            theme: o.theme,
            grading_mode: o.grading_mode.as_str(),
            new_per_day: o.new_per_day,
            reviews_per_day: o.reviews_per_day,
            timezone: o.timezone,
            day_cutoff_hour: o.day_cutoff_hour,
            desired_retention: o.desired_retention,
            card_count: o.card_count,
            extra: crate::ext::JsonFields::new(),
        }
    }
}

/// What every successful sign-in and refresh returns.
#[derive(Debug, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: &'static str,
    /// Seconds until `access_token` expires.
    pub expires_in: i64,
    pub user: Me,
}

impl TokenPair {
    pub fn new(access_token: String, refresh_token: String, user: Me) -> Self {
        TokenPair {
            access_token,
            refresh_token,
            token_type: "Bearer",
            expires_in: crate::oauth::ACCESS_TTL_MS / 1000,
            user,
        }
    }
}
