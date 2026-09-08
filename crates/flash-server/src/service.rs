//! Domain services: the single choke point through which every interface
//! (web UI, MCP tools, future API) reads and mutates study state. Handlers
//! translate protocol <-> these calls and nothing else.

use std::sync::Arc;

use flash_core::queue::{order_queue, StudyScope};
use flash_core::{
    validate_card_text, CardId, CardState, DeckId, GradingMode, MediaId, NoteId, Rating, Scheduler,
    SessionId, UserId, UserSettings,
};
use flash_store::import::ImportRow;
use flash_store::notes::{generate_cards, GeneratedCard, NoteType, MAX_FIELD_HTML};
use flash_store::{
    richtext, CardRow, DeckSummary, GrantRow, NoteSave, SessionStats, Store, StoreError,
};
use jiff::{tz::TimeZone, Timestamp, Zoned};

use crate::policy::{AddSource, Admission, CardPolicy, Unlimited};
use crate::state::HeavyPermit;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Invalid(String),
    #[error("scheduler: {0}")]
    Scheduler(String),
    /// Our own failure (a template, a serializer, a build step): the
    /// detail is for the log, and every surface shows "internal error".
    #[error("internal error")]
    Internal(String),
    /// An add the card policy refused; `message` is the policy's own
    /// sentence and reaches AI assistants verbatim through MCP.
    #[error("{message}")]
    OverCap {
        current: u32,
        cap: u32,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, ServiceError>;

#[derive(Debug, Clone, PartialEq)]
pub struct CardFront {
    pub card_id: CardId,
    pub front: String,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub grading_mode: GradingMode,
    pub cards_due: u32,
    /// Eligible cards today's new/review limits kept out of the queue.
    pub held_by_limit: u32,
    pub first_card: Option<CardFront>,
    /// The card after `first_card` in today's queue, so the UI can warm
    /// its media before it's shown. None when the queue has fewer than two.
    pub next_card_id: Option<CardId>,
}

#[derive(Debug, Clone)]
pub struct SubmitResult {
    pub next_card: Option<CardFront>,
    /// The card after `next_card` in the queue (media warm-up); None when
    /// there isn't one.
    pub next_card_id: Option<CardId>,
    pub remaining: u32,
    /// Eligible cards today's new/review limits kept out of the queue.
    pub held_by_limit: u32,
}

/// What a study surface shows for a card: rich HTML when the card has
/// it, whether it asks for a typed answer, and which images to warm —
/// this card's still-hidden back, and the next card's both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudyCard {
    pub card_id: CardId,
    pub front: String,
    pub front_html: Option<String>,
    pub wants_typing: bool,
    pub preload_media: Vec<i64>,
    pub prefetch_media: Vec<i64>,
}

/// One run of a typed answer, right or wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    pub text: String,
    pub ok: bool,
}

/// A revealed card: the front again (the surface re-renders it), the
/// back, and the typed-answer comparison when one was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reveal {
    pub card_id: CardId,
    pub front: String,
    pub front_html: Option<String>,
    pub back: String,
    pub back_html: Option<String>,
    pub typed_diff: Option<Vec<DiffSpan>>,
}

/// The Today page in one read.
#[derive(Debug, Clone)]
pub struct Dashboard {
    pub counts: QueueCounts,
    /// Account-wide "more new cards today" already granted.
    pub boost_today: u32,
    pub decks: Vec<DeckSummary>,
    /// The "Connect your AI" card, until the user dismisses it.
    pub show_connect_cta: bool,
}

/// One page of a deck's cards, as the lists show them.
#[derive(Debug, Clone)]
pub struct CardsPage {
    pub rows: Vec<CardRow>,
    pub page: u32,
    pub pages: u32,
    pub total: u32,
}

/// A deck's settings in one read: the deck, its limit overrides beside
/// the account defaults they would inherit, today's boost, and the live
/// card count (sharing is refused at zero).
#[derive(Debug, Clone)]
pub struct DeckDetail {
    pub deck: DeckSummary,
    pub limits: flash_store::DeckLimits,
    pub default_new: u32,
    pub default_reviews: u32,
    pub boost_today: u32,
    pub card_count: u32,
}

/// The cap the store must re-check (None = unlimited) plus the validated
/// texts, ready to insert.
type AdmittedCards = (Option<u32>, Vec<(flash_core::CardText, Vec<String>)>);

/// How a card relates to its note, for list badges: "cloze 2",
/// "reversed", "typed"; None for a plain basic card.
pub fn card_kind(card: &CardRow) -> Option<String> {
    match (card.cloze_index, card.ord, card.type_answer.is_some()) {
        (Some(n), _, _) => Some(format!("cloze {n}")),
        (None, 1, _) => Some("reversed".into()),
        (None, _, true) => Some("typed".into()),
        _ => None,
    }
}

/// The queue entry following `current`, for media warm-up.
fn lookahead(queue: &[CardId], current: Option<CardId>) -> Option<CardId> {
    let current = current?;
    let at = queue.iter().position(|&id| id == current)?;
    queue.get(at + 1).copied()
}

/// Where a review came from: the surface (`web`, `mobile`, `mcp`) and,
/// when the surface has several clients, which one (an MCP client's
/// name, the app's platform).
#[derive(Debug, Clone, Copy)]
pub struct ReviewOrigin<'a> {
    pub source: &'a str,
    pub client: Option<&'a str>,
}

impl<'a> ReviewOrigin<'a> {
    pub const WEB: ReviewOrigin<'static> = ReviewOrigin {
        source: "web",
        client: None,
    };

    pub fn new(source: &'a str, client: Option<&'a str>) -> Self {
        ReviewOrigin { source, client }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QueueCounts {
    pub due: u32,
    pub new_available: u32,
    pub reviewed_today: u32,
}

#[derive(Debug, Clone)]
pub struct DayCount {
    pub label: String,
    pub count: u32,
    /// Bar height as a percentage of the busiest day (chart-ready).
    pub pct: u32,
}

/// One day cell of the yearly heatmap.
#[derive(Debug, Clone)]
pub struct HeatCell {
    pub count: u32,
    /// 0 = empty, 1-4 = intensity relative to the busiest day.
    pub level: u8,
    /// MM/DD, for the tooltip.
    pub label: String,
    /// After today: `count` is cards due, not reviews done.
    pub future: bool,
    pub today: bool,
}

/// A run of heatmap columns sharing a month, for the label row.
#[derive(Debug, Clone)]
pub struct MonthSpan {
    pub label: String,
    pub weeks: u32,
}

#[derive(Debug, Clone)]
pub struct StatsData {
    pub reviews_30d: u32,
    /// None until there are mature (review-phase) reviews to measure.
    pub retention_pct: Option<u32>,
    pub streak: u32,
    pub longest_streak: u32,
    /// Reviews per day since the first review (window-capped at a year).
    pub daily_avg: u32,
    /// Share of days with at least one review, same denominator.
    pub days_learned_pct: u32,
    pub total_reviews: u32,
    pub active_cards: u32,
    pub mature_cards: u32,
    pub days: Vec<DayCount>,
    pub upcoming: Vec<DayCount>,
    /// 53 week columns of 7 cells each (Sunday first), oldest week first.
    pub heat_weeks: Vec<Vec<HeatCell>>,
    pub heat_months: Vec<MonthSpan>,
}

/// The account as a client's "me": profile, plan, theme, and which login
/// methods it holds (so settings can refuse to remove the last one).
#[derive(Debug, Clone)]
pub struct AccountOverview {
    pub id: UserId,
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub theme: String,
    pub grading_mode: GradingMode,
    pub has_password: bool,
    pub passkeys: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PasskeyInfo {
    pub id: i64,
    pub label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Everything the settings page shows, in one read.
#[derive(Debug, Clone)]
pub struct SettingsOverview {
    pub display_name: String,
    pub email: Option<String>,
    pub role: String,
    pub theme: String,
    pub grading_mode: GradingMode,
    pub new_per_day: u32,
    pub reviews_per_day: u32,
    pub timezone: String,
    pub day_cutoff_hour: u8,
    pub desired_retention: f32,
    pub has_password: bool,
    pub passkeys: Vec<PasskeyInfo>,
    /// Live cards across every deck.
    pub card_count: u32,
}

/// The knobs that decide when "today" starts and how FSRS schedules.
/// `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StudySettingsPatch {
    pub timezone: Option<String>,
    pub day_cutoff_hour: Option<u8>,
    pub desired_retention: Option<f32>,
}

/// Sync by design; the async layer wraps calls in spawn_blocking.
#[derive(Clone)]
pub struct Services {
    store: Arc<Store>,
    policy: Arc<dyn CardPolicy>,
}

impl Services {
    /// Unlimited cards: the open server.
    pub fn new(store: Arc<Store>) -> Self {
        Self::with_policy(store, Arc::new(Unlimited))
    }

    pub fn with_policy(store: Arc<Store>, policy: Arc<dyn CardPolicy>) -> Self {
        Self { store, policy }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn over_cap(&self, current: u32, cap: u32) -> ServiceError {
        ServiceError::OverCap {
            current,
            cap,
            message: self.policy.over_cap_message(current, cap),
        }
    }

    /// The store re-checks the cap inside its insert transaction; a race
    /// it loses surfaces as the same refusal the gate would have given.
    fn cap_error(&self, e: StoreError) -> ServiceError {
        match e {
            StoreError::CapExceeded { current, cap } => self.over_cap(current, cap),
            other => other.into(),
        }
    }

    fn scheduler_for(&self, settings: &UserSettings) -> Result<Scheduler> {
        Scheduler::new(settings.fsrs_params.as_deref(), settings.desired_retention)
            .map_err(|e| ServiceError::Scheduler(e.to_string()))
    }

    /// Start of the user's current study day (e.g. 4am local), for the
    /// daily new-card budget.
    fn study_day_start_ms(settings: &UserSettings, now_ms: i64) -> i64 {
        let tz = TimeZone::get(&settings.timezone).unwrap_or(TimeZone::UTC);
        let now = Timestamp::from_millisecond(now_ms).unwrap_or(Timestamp::UNIX_EPOCH);
        let local: Zoned = now.to_zoned(tz);
        let cutoff = local
            .with()
            .hour(settings.day_cutoff_hour as i8)
            .minute(0)
            .second(0)
            .subsec_nanosecond(0)
            .build()
            .unwrap_or_else(|_| local.clone());
        let day_start = if cutoff.timestamp() > now {
            cutoff.yesterday().unwrap_or(cutoff)
        } else {
            cutoff
        };
        day_start.timestamp().as_millisecond()
    }

    /// Today's remaining allowances, Anki-style: each deck's own new/review
    /// limit (or the account default) minus what that deck consumed today,
    /// plus any "more new cards today" boost; the account totals cap the
    /// sum. An account boost raises the default, so decks that inherit it
    /// get it too. The free-plan clamp applies to the account new total
    /// (unseen cards stop entering once rotation reaches the cap). Shared by
    /// the queue builder, dashboard counts, and deck summaries so they agree.
    fn budgets(
        &self,
        user: UserId,
        settings: &UserSettings,
        now_ms: i64,
    ) -> Result<flash_core::queue::Budgets> {
        let day_start = Self::study_day_start_ms(settings, now_ms);
        let consumed = self.store.daily_consumption_by_deck(user, day_start)?;
        let (introduced_total, reviewed_total) = consumed
            .values()
            .fold((0u32, 0u32), |acc, (n, r)| (acc.0 + n, acc.1 + r));
        let account_boost = active_boost(settings.boost_new, settings.boost_day, day_start);
        let review_total = settings.reviews_per_day.saturating_sub(reviewed_total);

        // A deck boost must actually yield more cards even when the account
        // allowance is already spent (the usual moment someone asks for
        // more), so today's deck boosts lift the account ceiling too.
        let mut deck_boosts_total = 0u32;
        let mut per_deck = std::collections::HashMap::new();
        for (deck, limits) in self.store.all_deck_limits(user)? {
            let (n, r) = consumed.get(&deck).copied().unwrap_or((0, 0));
            let deck_boost = active_boost(limits.boost_new, limits.boost_day, day_start);
            deck_boosts_total += deck_boost;
            let new_limit = match limits.new_per_day {
                Some(own) => own,
                None => settings.new_per_day + account_boost,
            } + deck_boost;
            let review_limit = limits.reviews_per_day.unwrap_or(settings.reviews_per_day);
            per_deck.insert(
                deck,
                flash_core::queue::DeckBudget {
                    new: new_limit.saturating_sub(n),
                    reviews: review_limit.saturating_sub(r),
                },
            );
        }
        let account_new = (settings.new_per_day + account_boost + deck_boosts_total)
            .saturating_sub(introduced_total);

        let new_total = self
            .policy
            .new_budget(&self.store, user, account_new, now_ms)?;
        Ok(flash_core::queue::Budgets {
            new_total,
            review_total,
            per_deck,
        })
    }

    /// Ordered due-card ids for a scope, respecting today's limits, plus
    /// how many otherwise-eligible cards (new, or due now) today's limits
    /// held back. The held count lets the study surfaces explain an empty
    /// or short queue instead of implying the user is done.
    fn build_queue(
        &self,
        user: UserId,
        scope: &StudyScope,
        now_ms: i64,
    ) -> Result<(Vec<CardId>, u32)> {
        let settings = self.store.get_settings(user)?;
        let budgets = self.budgets(user, &settings, now_ms)?;
        let entries = self.store.queue_entries(user, scope, now_ms)?;
        let eligible = entries
            .iter()
            .filter(|e| e.phase == flash_core::Phase::New || e.due_ms <= now_ms)
            .count();
        let ordered = order_queue(&entries, now_ms, &budgets);
        let held = eligible.saturating_sub(ordered.len()) as u32;
        Ok((ordered, held))
    }

    // ---- Daily limits ----

    pub fn set_daily_limits(
        &self,
        user: UserId,
        new_per_day: u32,
        reviews_per_day: u32,
    ) -> Result<()> {
        if new_per_day > flash_core::settings::MAX_NEW_PER_DAY
            || reviews_per_day > flash_core::settings::MAX_REVIEWS_PER_DAY
        {
            return Err(ServiceError::Invalid("limit out of range".into()));
        }
        Ok(self
            .store
            .set_daily_limits(user, new_per_day, reviews_per_day)?)
    }

    pub fn deck_limits(&self, user: UserId, deck: DeckId) -> Result<flash_store::DeckLimits> {
        Ok(self.store.get_deck_limits(user, deck)?)
    }

    /// Per-deck overrides; None = inherit the account default.
    pub fn set_deck_limits(
        &self,
        user: UserId,
        deck: DeckId,
        new_per_day: Option<u32>,
        reviews_per_day: Option<u32>,
    ) -> Result<()> {
        if new_per_day.is_some_and(|n| n > flash_core::settings::MAX_NEW_PER_DAY)
            || reviews_per_day.is_some_and(|n| n > flash_core::settings::MAX_REVIEWS_PER_DAY)
        {
            return Err(ServiceError::Invalid("limit out of range".into()));
        }
        Ok(self
            .store
            .set_deck_limits(user, deck, new_per_day, reviews_per_day)?)
    }

    pub fn rename_deck(&self, user: UserId, deck: DeckId, name: &str) -> Result<()> {
        Ok(self.store.rename_deck(user, deck, name)?)
    }

    /// "More new cards today": adds `extra` to today's new-card allowance
    /// for one deck, or for the account default when `deck` is None.
    /// Returns the total boost now active for today.
    pub fn boost_new_today(
        &self,
        user: UserId,
        deck: Option<DeckId>,
        extra: u32,
        now_ms: i64,
    ) -> Result<u32> {
        if extra == 0 || extra > flash_core::settings::MAX_BOOST {
            return Err(ServiceError::Invalid("boost out of range".into()));
        }
        let settings = self.store.get_settings(user)?;
        let day = Self::study_day_start_ms(&settings, now_ms);
        let current = match deck {
            Some(d) => {
                let limits = self.store.get_deck_limits(user, d)?;
                active_boost(limits.boost_new, limits.boost_day, day)
            }
            None => active_boost(settings.boost_new, settings.boost_day, day),
        };
        let total = (current + extra).min(flash_core::settings::MAX_BOOST);
        match deck {
            Some(d) => self.store.set_deck_boost(user, d, total, day)?,
            None => self.store.set_account_boost(user, total, day)?,
        }
        Ok(total)
    }

    /// Today's active boost for a deck (or the account when None).
    pub fn boost_today(&self, user: UserId, deck: Option<DeckId>, now_ms: i64) -> Result<u32> {
        let settings = self.store.get_settings(user)?;
        let day = Self::study_day_start_ms(&settings, now_ms);
        Ok(match deck {
            Some(d) => {
                let l = self.store.get_deck_limits(user, d)?;
                active_boost(l.boost_new, l.boost_day, day)
            }
            None => active_boost(settings.boost_new, settings.boost_day, day),
        })
    }

    fn card_front(&self, user: UserId, card: CardId) -> Result<Option<CardFront>> {
        Ok(self.store.get_card(user, card)?.map(|c| CardFront {
            card_id: c.id,
            front: c.front,
        }))
    }

    // ---- Study flow ----

    pub fn start_session(
        &self,
        user: UserId,
        scope: StudyScope,
        now_ms: i64,
    ) -> Result<SessionInfo> {
        // A session scoped to a deck is a session on one of the user's
        // decks; a foreign id is not found, not an empty session.
        if let StudyScope::Deck(deck) = scope {
            self.store.get_deck_limits(user, deck)?;
        }
        let (queue, held_by_limit) = self.build_queue(user, &scope, now_ms)?;
        let session_id = self.store.start_session(user, &scope, now_ms)?;
        let settings = self.store.get_settings(user)?;
        let first_card = match queue.first() {
            Some(&id) => self.card_front(user, id)?,
            None => None,
        };
        Ok(SessionInfo {
            session_id,
            grading_mode: settings.grading_mode,
            cards_due: queue.len() as u32,
            held_by_limit,
            first_card,
            next_card_id: queue.get(1).copied(),
        })
    }

    /// Records a review and returns the next card in the same call — the
    /// voice loop is one tool call per card.
    pub fn submit_review(
        &self,
        user: UserId,
        session: SessionId,
        card: CardId,
        rating: Rating,
        origin: ReviewOrigin<'_>,
        now_ms: i64,
    ) -> Result<SubmitResult> {
        let scope = self.store.get_session_scope(user, session)?;
        let settings = self.store.get_settings(user)?;
        let state = self.store.get_card_state(user, card)?;

        // Transport-level retries (Streamable HTTP clients may re-POST)
        // must not double-record: a repeat submission for a card reviewed
        // seconds ago is answered idempotently with the current queue.
        const RETRY_WINDOW_MS: i64 = 5_000;
        if state
            .last_review_ms
            .is_some_and(|t| (0..RETRY_WINDOW_MS).contains(&now_ms.saturating_sub(t)))
        {
            let (queue, held_by_limit) = self.build_queue(user, &scope, now_ms)?;
            let next = queue.iter().find(|&&id| id != card).or(queue.first());
            let next_card = match next {
                Some(&id) => self.card_front(user, id)?,
                None => None,
            };
            let next_card_id = lookahead(&queue, next.copied());
            return Ok(SubmitResult {
                remaining: queue.len() as u32,
                held_by_limit,
                next_card,
                next_card_id,
            });
        }

        let outcome = self
            .scheduler_for(&settings)?
            .review(&state, rating, now_ms)
            .map_err(|e| ServiceError::Scheduler(e.to_string()))?;
        self.store.record_review(
            user,
            card,
            &outcome,
            now_ms,
            origin.source,
            origin.client,
            Some(session),
        )?;

        let (queue, held_by_limit) = self.build_queue(user, &scope, now_ms)?;
        // The just-reviewed card may already be due again (learning step);
        // don't hand it straight back.
        let next = queue.iter().find(|&&id| id != card).or(queue.first());
        let next_card = match next {
            Some(&id) => self.card_front(user, id)?,
            None => None,
        };
        let next_card_id = lookahead(&queue, next.copied());
        Ok(SubmitResult {
            remaining: queue.len() as u32,
            held_by_limit,
            next_card,
            next_card_id,
        })
    }

    /// The card's back text, for reveal ("show me the answer").
    pub fn reveal(&self, user: UserId, card: CardId) -> Result<String> {
        Ok(self
            .store
            .get_card(user, card)?
            .ok_or(StoreError::NotFound("card"))?
            .back)
    }

    /// The study view of `front`, with `next` (the queue's following card)
    /// contributing its images to the prefetch list.
    pub fn study_card(
        &self,
        user: UserId,
        front: CardFront,
        next: Option<CardId>,
    ) -> Result<StudyCard> {
        let row = self
            .store
            .get_card(user, front.card_id)?
            .ok_or(StoreError::NotFound("card"))?;
        let preload_media = row
            .back_html
            .as_deref()
            .map(richtext::media_image_ids)
            .unwrap_or_default();
        let prefetch_media = match next {
            Some(id) => self.card_image_ids(user, id)?,
            None => Vec::new(),
        };
        Ok(StudyCard {
            card_id: front.card_id,
            front: front.front,
            front_html: row.front_html,
            wants_typing: row.type_answer.is_some(),
            preload_media,
            prefetch_media,
        })
    }

    /// Every image id on both sides of a card, for prefetching.
    fn card_image_ids(&self, user: UserId, card: CardId) -> Result<Vec<i64>> {
        let Some(row) = self.store.get_card(user, card)? else {
            return Ok(Vec::new());
        };
        let mut ids = Vec::new();
        for html in [row.front_html.as_deref(), row.back_html.as_deref()]
            .into_iter()
            .flatten()
        {
            for id in richtext::media_image_ids(html) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// Reveal for the interactive surfaces: the back, plus the diff of
    /// what the user typed when the card asked for it.
    pub fn reveal_card(&self, user: UserId, card: CardId, typed: Option<&str>) -> Result<Reveal> {
        let row = self
            .store
            .get_card(user, card)?
            .ok_or(StoreError::NotFound("card"))?;
        let typed_diff = match (&row.type_answer, typed.map(str::trim)) {
            (Some(expected), Some(typed)) if !typed.is_empty() => Some(type_diff(expected, typed)),
            _ => None,
        };
        Ok(Reveal {
            card_id: row.id,
            front: row.front,
            front_html: row.front_html,
            back: row.back,
            back_html: row.back_html,
            typed_diff,
        })
    }

    /// What a session was started on, so an empty queue can offer the
    /// right boost (that deck's, or the account's).
    pub fn session_scope(&self, user: UserId, session: SessionId) -> Result<StudyScope> {
        Ok(self.store.get_session_scope(user, session)?)
    }

    /// Start of the user's current study day, for anything that runs on
    /// the same clock as the daily budgets (the reminder scheduler).
    pub fn study_day_start(&self, user: UserId, now_ms: i64) -> Result<i64> {
        let settings = self.store.get_settings(user)?;
        Ok(Self::study_day_start_ms(&settings, now_ms))
    }

    // ---- Today ----

    pub fn dashboard(&self, user: UserId, now_ms: i64) -> Result<Dashboard> {
        Ok(Dashboard {
            counts: self.queue_counts(user, &StudyScope::All, now_ms)?,
            boost_today: self.boost_today(user, None, now_ms)?,
            decks: self.list_decks(user, now_ms)?,
            show_connect_cta: !self.store.connect_cta_hidden(user)?,
        })
    }

    pub fn dismiss_connect_cta(&self, user: UserId) -> Result<()> {
        Ok(self.store.set_connect_cta_hidden(user, true)?)
    }

    pub fn end_session(
        &self,
        user: UserId,
        session: SessionId,
        now_ms: i64,
    ) -> Result<SessionStats> {
        self.store.end_session(user, session, now_ms)?;
        Ok(self.store.session_stats(user, session)?)
    }

    /// Dashboard numbers without starting a session — derived from the
    /// same ordered queue a session would get, so they always agree.
    pub fn queue_counts(
        &self,
        user: UserId,
        scope: &StudyScope,
        now_ms: i64,
    ) -> Result<QueueCounts> {
        let settings = self.store.get_settings(user)?;
        let day_start = Self::study_day_start_ms(&settings, now_ms);
        let budgets = self.budgets(user, &settings, now_ms)?;
        let entries = self.store.queue_entries(user, scope, now_ms)?;
        let ordered = order_queue(&entries, now_ms, &budgets);
        let by_id: std::collections::HashMap<CardId, &flash_core::queue::QueueEntry> =
            entries.iter().map(|e| (e.card_id, e)).collect();
        let mut due = 0u32;
        let mut new_available = 0u32;
        for id in &ordered {
            if let Some(e) = by_id.get(id) {
                if e.phase == flash_core::Phase::New {
                    new_available += 1;
                } else if e.due_ms <= now_ms {
                    due += 1;
                }
            }
        }
        let reviewed_today = self.store.reviews_since(user, day_start)?.len() as u32;
        Ok(QueueCounts {
            due,
            new_available,
            reviewed_today,
        })
    }

    /// The Stats page's numbers: a scan of the whole review history, so a
    /// heavy job that needs its permit.
    pub fn stats(&self, permit: &HeavyPermit, now_ms: i64) -> Result<StatsData> {
        let user = permit.user();
        let settings = self.store.get_settings(user)?;
        let tz = TimeZone::get(&settings.timezone).unwrap_or(TimeZone::UTC);
        // Shift by the study-day cutoff so a 2am review counts toward the
        // previous day, matching the queue's notion of "today".
        let cutoff_ms = i64::from(settings.day_cutoff_hour) * 3_600_000;
        let local_of = |ts_ms: i64| -> Zoned {
            Timestamp::from_millisecond(ts_ms - cutoff_ms)
                .unwrap_or(Timestamp::UNIX_EPOCH)
                .to_zoned(tz.clone())
        };
        let day_of = |ts_ms: i64| -> (i64, String) {
            let shifted = ts_ms - cutoff_ms;
            let local = local_of(ts_ms);
            // Epoch-day index in local time keeps ordering; label is MM/DD.
            let idx = (shifted + i64::from(local.offset().seconds()) * 1000)
                / flash_core::scheduler::MS_PER_DAY;
            (idx, format!("{:02}/{:02}", local.month(), local.day()))
        };

        const DAY: i64 = flash_core::scheduler::MS_PER_DAY;
        let reviews = self.store.reviews_since(user, now_ms - 365 * DAY)?;
        let mut per_day: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
        let mut review_reviews = 0u32;
        let mut review_recalled = 0u32;
        for &(ts, rating, phase_before) in &reviews {
            *per_day.entry(day_of(ts).0).or_default() += 1;
            if phase_before == 2 && ts >= now_ms - 30 * DAY {
                review_reviews += 1;
                if rating > 1 {
                    review_recalled += 1;
                }
            }
        }

        let (today_idx, _) = day_of(now_ms);
        let mut streak = 0u32;
        let mut idx = today_idx;
        // Today counts if studied; otherwise the streak may end yesterday.
        if !per_day.contains_key(&idx) {
            idx -= 1;
        }
        while per_day.contains_key(&idx) {
            streak += 1;
            idx -= 1;
        }

        let mut longest_streak = 0u32;
        {
            let mut keys: Vec<i64> = per_day.keys().copied().collect();
            keys.sort_unstable();
            let mut run = 0u32;
            let mut prev: Option<i64> = None;
            for k in keys {
                run = if prev == Some(k - 1) { run + 1 } else { 1 };
                prev = Some(k);
                longest_streak = longest_streak.max(run);
            }
        }

        let mut days = Vec::with_capacity(30);
        for offset in (0..30).rev() {
            let ts = now_ms - offset * DAY;
            let (idx, label) = day_of(ts);
            days.push(DayCount {
                label,
                count: per_day.get(&idx).copied().unwrap_or(0),
                pct: 0,
            });
        }
        let max = days.iter().map(|d| d.count).max().unwrap_or(0).max(1);
        for day in &mut days {
            day.pct = day.count * 100 / max;
        }
        let reviews_30d = reviews
            .iter()
            .filter(|(ts, _, _)| *ts >= now_ms - 30 * DAY)
            .count() as u32;

        let mut upcoming_map: std::collections::HashMap<i64, u32> =
            std::collections::HashMap::new();
        for due in self.store.due_between(user, now_ms, now_ms + 7 * DAY)? {
            *upcoming_map.entry(day_of(due).0).or_default() += 1;
        }
        let mut upcoming = Vec::with_capacity(7);
        for offset in 0..7 {
            let ts = now_ms + offset * DAY;
            let (idx, label) = day_of(ts);
            upcoming.push(DayCount {
                label: if offset == 0 {
                    "Today".to_string()
                } else {
                    label
                },
                count: upcoming_map.get(&idx).copied().unwrap_or(0),
                pct: 0,
            });
        }

        // ---- Yearly heatmap: 53 week columns (Sunday-first), today's week
        // is column 46, then six columns of upcoming due load. ----
        const WEEKS: i64 = 53;
        const PAST_WEEKS: i64 = 46;
        // Epoch day 0 (1970-01-01) was a Thursday; +4 makes Sunday zero.
        let today_dow = (today_idx + 4).rem_euclid(7);
        let first_idx = today_idx - today_dow - PAST_WEEKS * 7;
        let last_idx = first_idx + WEEKS * 7 - 1;

        let mut future_map: std::collections::HashMap<i64, u32> = std::collections::HashMap::new();
        let horizon_ms = now_ms + (last_idx - today_idx + 2) * DAY;
        for due in self.store.due_between(user, now_ms, horizon_ms)? {
            let idx = day_of(due).0;
            if idx > today_idx {
                *future_map.entry(idx).or_default() += 1;
            }
        }
        let past_max = per_day
            .iter()
            .filter(|(k, _)| **k >= first_idx)
            .map(|(_, v)| *v)
            .max()
            .unwrap_or(0)
            .max(1);
        let future_max = future_map.values().copied().max().unwrap_or(0).max(1);
        let level = |count: u32, max: u32| -> u8 {
            if count == 0 {
                0
            } else {
                (count * 4).div_ceil(max).clamp(1, 4) as u8
            }
        };

        let mut heat_weeks = Vec::with_capacity(WEEKS as usize);
        let mut heat_months: Vec<MonthSpan> = Vec::new();
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        for week in 0..WEEKS {
            let mut cells = Vec::with_capacity(7);
            let col_start_idx = first_idx + week * 7;
            let col_month = {
                let ts = now_ms + (col_start_idx - today_idx) * DAY;
                MONTHS[(local_of(ts).month() as usize - 1) % 12]
            };
            match heat_months.last_mut() {
                Some(span) if span.label == col_month => span.weeks += 1,
                _ => heat_months.push(MonthSpan {
                    label: col_month.to_string(),
                    weeks: 1,
                }),
            }
            for dow in 0..7 {
                let cell_idx = col_start_idx + dow;
                let ts = now_ms + (cell_idx - today_idx) * DAY;
                let (_, label) = day_of(ts);
                let future = cell_idx > today_idx;
                let count = if future {
                    future_map.get(&cell_idx).copied().unwrap_or(0)
                } else {
                    per_day.get(&cell_idx).copied().unwrap_or(0)
                };
                cells.push(HeatCell {
                    count,
                    level: level(count, if future { future_max } else { past_max }),
                    label,
                    future,
                    today: cell_idx == today_idx,
                });
            }
            heat_weeks.push(cells);
        }
        // Blank cramped month labels (a month spanning <3 columns).
        for span in &mut heat_months {
            if span.weeks < 3 {
                span.label = String::new();
            }
        }

        let (total_reviews, first_review_ms) = self.store.review_totals(user)?;
        let (active_cards, mature_cards) = self.store.card_totals(user)?;
        let denom_days = first_review_ms
            .map(|first| (today_idx - day_of(first).0 + 1).clamp(1, 365) as u32)
            .unwrap_or(1);
        let year_reviews = reviews.len() as u32;

        Ok(StatsData {
            reviews_30d,
            retention_pct: (review_reviews > 0).then(|| review_recalled * 100 / review_reviews),
            streak,
            longest_streak,
            daily_avg: year_reviews / denom_days,
            days_learned_pct: (per_day.len() as u32 * 100 / denom_days).min(100),
            total_reviews,
            active_cards,
            mature_cards,
            days,
            upcoming,
            heat_weeks,
            heat_months,
        })
    }

    // ---- Card management ----

    pub fn create_deck(
        &self,
        user: UserId,
        name: &str,
        description: &str,
        now_ms: i64,
    ) -> Result<DeckId> {
        // The per-account deck cap is the store's: it holds for every
        // caller of `create_deck`, imports included.
        Ok(self.store.create_deck(user, name, description, now_ms)?)
    }

    /// Decks with today's *budgeted* due/new counts (what studying that
    /// deck would actually serve), not raw totals.
    pub fn list_decks(&self, user: UserId, now_ms: i64) -> Result<Vec<DeckSummary>> {
        let settings = self.store.get_settings(user)?;
        let budgets = self.budgets(user, &settings, now_ms)?;
        let mut decks = self.store.list_decks(user, now_ms)?;
        for d in &mut decks {
            let own =
                budgets
                    .per_deck
                    .get(&d.id)
                    .copied()
                    .unwrap_or(flash_core::queue::DeckBudget {
                        new: u32::MAX,
                        reviews: u32::MAX,
                    });
            d.new_count = d.new_count.min(own.new).min(budgets.new_total);
            d.due_count = d.due_count.min(own.reviews).min(budgets.review_total);
        }
        Ok(decks)
    }

    /// One of the user's decks by id, with today's budgeted counts.
    pub fn deck_by_id(&self, user: UserId, deck: DeckId, now_ms: i64) -> Result<DeckSummary> {
        self.list_decks(user, now_ms)?
            .into_iter()
            .find(|d| d.id == deck)
            .ok_or(ServiceError::Store(StoreError::NotFound("deck")))
    }

    /// The deck settings page in one read.
    pub fn deck_detail(&self, user: UserId, deck: DeckId, now_ms: i64) -> Result<DeckDetail> {
        let summary = self.deck_by_id(user, deck, now_ms)?;
        let settings = self.store.get_settings(user)?;
        Ok(DeckDetail {
            limits: self.store.get_deck_limits(user, deck)?,
            default_new: settings.new_per_day,
            default_reviews: settings.reviews_per_day,
            boost_today: self.boost_today(user, Some(deck), now_ms)?,
            card_count: self.store.count_cards(user, Some(deck), None)?,
            deck: summary,
        })
    }

    /// The admission gate plus text validation shared by every
    /// interactive add: all-or-nothing at the free-plan cap (the store
    /// re-checks inside its insert transaction).
    fn admitted_cards(
        &self,
        user: UserId,
        cards: &[(String, String, Vec<String>)],
        now_ms: i64,
    ) -> Result<AdmittedCards> {
        let cap = self.admit_cards(user, cards.len() as u32, AddSource::Interactive, now_ms)?;
        let validated = cards
            .iter()
            .map(|(front, back, tags)| {
                validate_card_text(front, back)
                    .map(|text| (text, tags.clone()))
                    .map_err(ServiceError::Invalid)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((cap, validated))
    }

    /// Batch create into a deck named `deck_name` (created if missing) —
    /// the MCP tools' path, where decks are spoken, not clicked.
    pub fn create_cards(
        &self,
        user: UserId,
        deck_name: &str,
        cards: &[(String, String, Vec<String>)],
        now_ms: i64,
    ) -> Result<Vec<CardId>> {
        let (cap, validated) = self.admitted_cards(user, cards, now_ms)?;
        let deck = match self.store.find_deck_by_name(user, deck_name)? {
            Some(id) => id,
            None => self.store.create_deck(user, deck_name, "", now_ms)?,
        };
        self.store
            .create_cards(user, deck, &validated, cap, now_ms)
            .map_err(|e| self.cap_error(e))
    }

    /// Batch create into an existing deck of the user's — the quick-add
    /// form on the web and in the app.
    pub fn create_cards_in_deck(
        &self,
        user: UserId,
        deck: DeckId,
        cards: &[(String, String, Vec<String>)],
        now_ms: i64,
    ) -> Result<Vec<CardId>> {
        self.deck_by_id(user, deck, now_ms)?;
        let (cap, validated) = self.admitted_cards(user, cards, now_ms)?;
        self.store
            .create_cards(user, deck, &validated, cap, now_ms)
            .map_err(|e| self.cap_error(e))
    }

    /// One page of a deck's cards, optionally filtered by a search
    /// string; a deck that isn't the user's is not found.
    pub fn deck_cards_page(
        &self,
        user: UserId,
        deck: DeckId,
        search: Option<&str>,
        page: u32,
        per_page: u32,
        now_ms: i64,
    ) -> Result<CardsPage> {
        self.deck_by_id(user, deck, now_ms)?;
        let search = bounded_search(search)?;
        let per_page = per_page.clamp(1, 500);
        let (rows, total) = self.list_cards_page(user, deck, search, page.max(1), per_page)?;
        let pages = total.div_ceil(per_page).max(1);
        Ok(CardsPage {
            rows,
            page: page.max(1).min(pages),
            pages,
            total,
        })
    }

    /// One card of the user's.
    pub fn card(&self, user: UserId, card: CardId) -> Result<CardRow> {
        self.store
            .get_card(user, card)?
            .ok_or(ServiceError::Store(StoreError::NotFound("card")))
    }

    /// Admission gate for adding `adding` cards: asks the card policy and
    /// returns the cap the store must re-check inside its own transaction
    /// (None = unlimited). Public for the layers that insert cards by
    /// their own routes (copying a shared deck).
    pub fn admit_cards(
        &self,
        user: UserId,
        adding: u32,
        source: AddSource,
        now_ms: i64,
    ) -> Result<Option<u32>> {
        match self
            .policy
            .admit(&self.store, user, adding, source, now_ms)?
        {
            Admission::Allowed { recheck_cap } => Ok(recheck_cap),
            Admission::Denied { current, cap } => Err(self.over_cap(current, cap)),
        }
    }

    /// Commits a parsed import in one decision: the whole batch is either
    /// admitted (possibly starting the one-time overflow grace) or rejected
    /// before any insert. Rows without a deck land in `default_deck`. With
    /// `include_progress`, rows carrying Anki review history are replayed
    /// through FSRS so cards keep their real memory state instead of
    /// arriving as new. Returns the number of cards created.
    pub fn import_cards(
        &self,
        user: UserId,
        default_deck: &str,
        rows: Vec<ImportRow>,
        include_progress: bool,
        media_ids: Option<&std::collections::HashMap<String, flash_core::MediaId>>,
        now_ms: i64,
    ) -> Result<usize> {
        self.admit_cards(user, rows.len() as u32, AddSource::Import, now_ms)?;
        let scheduler = if include_progress {
            Some(self.scheduler_for(&self.store.get_settings(user)?)?)
        } else {
            None
        };
        let mut by_deck: std::collections::BTreeMap<String, Vec<ImportRow>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let deck = row
                .deck
                .clone()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or_else(|| default_deck.to_string());
            by_deck.entry(deck).or_default().push(row);
        }
        let mut created = 0usize;
        for (deck_name, batch) in by_deck {
            let validated = batch
                .iter()
                .map(|row| {
                    validate_card_text(&row.front, &row.back)
                        .map(|text| (text, row.tags.clone()))
                        .map_err(ServiceError::Invalid)
                })
                .collect::<Result<Vec<_>>>()?;
            let deck = match self.store.find_deck_by_name(user, &deck_name)? {
                Some(id) => id,
                None => self.store.create_deck(user, &deck_name, "", now_ms)?,
            };
            // The batch was admitted as a whole above, so no per-deck cap.
            let ids = self
                .store
                .create_cards(user, deck, &validated, None, now_ms)?;
            created += ids.len();
            if let Some(scheduler) = &scheduler {
                for (id, row) in ids.iter().zip(&batch) {
                    if row.reviews.is_empty() {
                        continue;
                    }
                    // A card whose history won't replay just stays new —
                    // never fail the whole import over one card.
                    if let Some(state) = replay_history(scheduler, &row.reviews, now_ms) {
                        if let Err(e) = self.store.update_card_state(user, *id, &state) {
                            tracing::warn!("progress import for card {id}: {e}");
                        }
                    }
                }
            }
            // Rich content, typing answers, cloze sources, suspension, and
            // media links — everything beyond the plain-text card.
            let mut extras_batch: Vec<(CardId, flash_store::CardExtras)> = Vec::new();
            let mut suspend_batch: Vec<CardId> = Vec::new();
            for (id, row) in ids.iter().zip(&batch) {
                if row.suspended {
                    suspend_batch.push(*id);
                }
                let extras = flash_store::CardExtras {
                    front_html: row.front_html.clone(),
                    back_html: row.back_html.clone(),
                    cloze_text: row.cloze_text.clone(),
                    cloze_index: row.cloze_index,
                    type_answer: row.type_answer.clone(),
                };
                if !extras.is_empty() {
                    extras_batch.push((*id, extras));
                }
                if let Some(map) = media_ids {
                    for media_ref in &row.media {
                        if let Some(mid) = map.get(&media_ref.filename) {
                            let _ = self.store.link_card_media(user, *id, *mid);
                        }
                    }
                }
            }
            if !extras_batch.is_empty() {
                self.store.set_card_extras(user, &extras_batch)?;
            }
            if !suspend_batch.is_empty() {
                self.store.suspend_cards(user, &suspend_batch, now_ms)?;
            }
        }
        Ok(created)
    }

    /// Adopts the FSRS-relevant settings an Anki package carried (opt-in
    /// at import). Params are validated by building a scheduler before
    /// anything is saved; invalid params are dropped, not fatal.
    pub fn adopt_imported_settings(
        &self,
        user: UserId,
        settings: &flash_store::import::ImportedSettings,
    ) -> Result<()> {
        let mut params = settings.fsrs_params.as_deref();
        if let Some(p) = params {
            if Scheduler::new(Some(p), settings.desired_retention.unwrap_or(0.9)).is_err() {
                tracing::warn!("imported FSRS params rejected by scheduler; skipping them");
                params = None;
            }
        }
        self.store.adopt_study_settings(
            user,
            settings.desired_retention,
            params,
            settings.new_per_day,
        )?;
        Ok(())
    }

    // ---- admin ----

    pub fn admin_users(&self, now_ms: i64) -> Result<Vec<flash_store::AdminUserRow>> {
        Ok(self.store.list_users_admin(now_ms)?)
    }

    /// A one-time member enrollment link, valid for seven days; returns
    /// the raw token (only its hash is stored) and the expiry.
    pub fn create_member_invite(
        &self,
        by: UserId,
        display_name: &str,
        now_ms: i64,
    ) -> Result<(String, i64)> {
        const INVITE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let name = display_name.trim();
        if name.is_empty() || name.len() > 80 {
            return Err(ServiceError::Invalid("name required".into()));
        }
        let token = crate::auth::new_token();
        let expires = now_ms + INVITE_TTL_MS;
        self.store.create_invite(
            &crate::auth::hash_token(&token),
            name,
            "member",
            Some(by),
            expires,
            now_ms,
        )?;
        Ok((token, expires))
    }

    // ---- account ----

    pub fn account_overview(&self, user: UserId) -> Result<AccountOverview> {
        let row = self.store.account_row(user)?;
        let settings = self.store.get_settings(user)?;
        Ok(AccountOverview {
            id: user,
            display_name: row.display_name,
            email: row.email,
            role: row.role,
            theme: row.theme,
            grading_mode: settings.grading_mode,
            has_password: self.store.get_password_hash(user)?.is_some(),
            passkeys: self.store.passkeys_for_user(user)?.len() as u32,
        })
    }

    pub fn settings_overview(&self, user: UserId, _now_ms: i64) -> Result<SettingsOverview> {
        let row = self.store.account_row(user)?;
        let settings = self.store.get_settings(user)?;
        Ok(SettingsOverview {
            display_name: row.display_name,
            email: row.email,
            role: row.role,
            theme: row.theme,
            grading_mode: settings.grading_mode,
            new_per_day: settings.new_per_day,
            reviews_per_day: settings.reviews_per_day,
            timezone: settings.timezone,
            day_cutoff_hour: settings.day_cutoff_hour,
            desired_retention: settings.desired_retention,
            has_password: self.store.get_password_hash(user)?.is_some(),
            passkeys: self
                .store
                .passkeys_for_user(user)?
                .into_iter()
                .map(|p| PasskeyInfo {
                    id: p.id,
                    label: p.label,
                    created_at: p.created_at,
                    last_used_at: p.last_used_at,
                })
                .collect(),
            card_count: self.store.count_cards(user, None, None)?,
        })
    }

    /// The apps and connectors holding live grants for the user, most
    /// recently used first.
    pub fn connected_apps(&self, user: UserId, now_ms: i64) -> Result<Vec<GrantRow>> {
        Ok(self.store.list_grants_for_user(user, now_ms)?)
    }

    /// Revokes every live grant one client holds for the user. Idempotent:
    /// a client with nothing left to revoke is already disconnected.
    pub fn disconnect_app(&self, user: UserId, client_id: &str, now_ms: i64) -> Result<()> {
        self.store.revoke_grant(client_id, user, now_ms)?;
        Ok(())
    }

    /// Same bounds as signup's name field.
    pub fn set_display_name(&self, user: UserId, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 80 {
            return Err(ServiceError::Invalid(
                "a name is required (up to 80 characters)".into(),
            ));
        }
        Ok(self.store.set_display_name(user, name)?)
    }

    pub fn set_grading_mode(&self, user: UserId, mode: &str) -> Result<()> {
        let Some(mode) = GradingMode::from_str(mode) else {
            return Err(ServiceError::Invalid(format!("unknown mode {mode:?}")));
        };
        Ok(self.store.set_grading_mode(user, mode)?)
    }

    /// Validated here so no surface can store a zone jiff can't resolve
    /// (the day cutoff and the reminder scheduler both depend on it).
    pub fn set_study_settings(&self, user: UserId, patch: &StudySettingsPatch) -> Result<()> {
        if let Some(tz) = &patch.timezone {
            if TimeZone::get(tz).is_err() {
                return Err(ServiceError::Invalid(format!("unknown timezone {tz:?}")));
            }
        }
        if patch.day_cutoff_hour.is_some_and(|h| h > 23) {
            return Err(ServiceError::Invalid(
                "day cutoff must be an hour from 0 to 23".into(),
            ));
        }
        if patch
            .desired_retention
            .is_some_and(|r| !(0.70..=0.99).contains(&r))
        {
            return Err(ServiceError::Invalid(
                "desired retention must be between 0.70 and 0.99".into(),
            ));
        }
        Ok(self.store.set_study_settings(
            user,
            patch.timezone.as_deref(),
            patch.day_cutoff_hour,
            patch.desired_retention,
        )?)
    }

    /// One of the five UI themes. The web also keeps a cookie copy so a
    /// page can render before any request reaches the store.
    pub fn set_theme(&self, user: UserId, theme: &str) -> Result<()> {
        if crate::auth::Theme::from_str(theme).is_none() {
            return Err(ServiceError::Invalid(format!("unknown theme {theme:?}")));
        }
        Ok(self.store.set_theme(user, theme)?)
    }

    /// Every live card with deck and tags — the export surface. Never
    /// gated on plan, cap, or grace.
    pub fn export_cards(&self, user: UserId) -> Result<Vec<flash_store::export::ExportCard>> {
        Ok(self.store.export_cards(user)?)
    }

    pub fn update_card(
        &self,
        user: UserId,
        card: CardId,
        front: &str,
        back: &str,
        now_ms: i64,
    ) -> Result<()> {
        let text = validate_card_text(front, back).map_err(ServiceError::Invalid)?;
        Ok(self.store.update_card(user, card, &text, now_ms)?)
    }

    pub fn delete_card(&self, user: UserId, card: CardId, now_ms: i64) -> Result<()> {
        Ok(self.store.delete_card(user, card, now_ms)?)
    }

    // ---- notes (the advanced editor) ----

    /// Sanitizes the editor's fields, expands them into cards, and checks
    /// every media reference resolves to the user's own upload.
    fn prepare_note(&self, user: UserId, input: &NoteInput) -> Result<PreparedNote> {
        if input.front_html.len() > MAX_FIELD_HTML || input.back_html.len() > MAX_FIELD_HTML {
            return Err(ServiceError::Invalid(format!(
                "a field exceeds {MAX_FIELD_HTML} bytes of HTML"
            )));
        }
        let front_html = richtext::sanitize_with_media(&input.front_html).trimmed();
        let back_html = richtext::sanitize_with_media(&input.back_html).trimmed();
        let generated = generate_cards(input.note_type, &front_html, &back_html)
            .map_err(ServiceError::Invalid)?;
        let mut media = richtext::media_ids(&front_html);
        media.extend(richtext::media_ids(&back_html));
        for id in media {
            if self.store.get_media(user, MediaId(id))?.is_none() {
                return Err(ServiceError::Invalid(format!(
                    "media {id} is not one of your uploads"
                )));
            }
        }
        let tags = normalize_tags(&input.tags);
        Ok(PreparedNote {
            front_html,
            back_html,
            generated,
            tags,
        })
    }

    /// Note-editor gate: `inserts` new cards go through `admit_cards`. An
    /// edit that adds none is never blocked — an over-cap account can
    /// still fix its existing cards — which is the policy's contract for
    /// an admission of zero: it only reports the cap the store re-checks.
    fn note_cap(&self, user: UserId, inserts: u32, now_ms: i64) -> Result<Option<u32>> {
        self.admit_cards(user, inserts, AddSource::Interactive, now_ms)
    }

    /// Creates a note (and its cards) in `deck` from the advanced editor.
    pub fn create_note(
        &self,
        user: UserId,
        deck: DeckId,
        input: &NoteInput,
        now_ms: i64,
    ) -> Result<(NoteId, Vec<CardId>)> {
        let p = self.prepare_note(user, input)?;
        let cap = self.note_cap(user, p.generated.len() as u32, now_ms)?;
        self.store
            .create_note(user, deck, &p.save(input.note_type, cap, now_ms))
            .map_err(|e| self.cap_error(e))
    }

    /// Saves the editor over an existing card: its note is rewritten and
    /// regenerated in place; a note-less card is adopted into a fresh
    /// note first (an imported cloze card brings its siblings).
    pub fn save_card_editor(
        &self,
        user: UserId,
        card: CardId,
        input: &NoteInput,
        now_ms: i64,
    ) -> Result<(NoteId, Vec<CardId>)> {
        let p = self.prepare_note(user, input)?;
        let note = match self.store.note_for_card(user, card)? {
            Some(row) => row,
            None => {
                let id = self.store.adopt_card_into_note(
                    user,
                    card,
                    input.note_type,
                    &p.front_html,
                    &p.back_html,
                    now_ms,
                )?;
                self.store
                    .get_note(user, id)?
                    .ok_or(ServiceError::Store(StoreError::NotFound("note")))?
            }
        };
        let inserts = p
            .generated
            .iter()
            .filter(|g| !note.cards.iter().any(|(ord, _)| *ord == g.ord))
            .count() as u32;
        let cap = self.note_cap(user, inserts, now_ms)?;
        let ids = self
            .store
            .update_note(user, note.id, &p.save(input.note_type, cap, now_ms))
            .map_err(|e| self.cap_error(e))?;
        Ok((note.id, ids))
    }

    /// What the editor opens with for `card`: its note's source fields, or
    /// a synthesized note for a standalone card (imported cloze/typed cards
    /// reconstruct their type from the rich columns).
    pub fn editor_seed(&self, user: UserId, card: CardId) -> Result<EditorSeed> {
        let row = self
            .store
            .get_card(user, card)?
            .ok_or(ServiceError::Store(StoreError::NotFound("card")))?;
        if let Some(note) = self.store.note_for_card(user, card)? {
            return Ok(EditorSeed {
                card_id: card,
                deck_id: note.deck_id,
                note_id: Some(note.id),
                note_type: note.note_type,
                front_html: note.front_html,
                back_html: note.back_html,
                tags: note.tags,
                sibling_count: note.cards.len(),
            });
        }
        let cloze = self.store.cloze_source(user, card)?;
        let (note_type, front_html, back_html) = match cloze {
            Some(source) => {
                let (text, extra) = source.split_once('\u{1f}').unwrap_or((&source, ""));
                (NoteType::Cloze, text_to_html(text), text_to_html(extra))
            }
            None => {
                let kind = if row.type_answer.is_some() {
                    NoteType::BasicTyped
                } else {
                    NoteType::Basic
                };
                (
                    kind,
                    row.front_html
                        .clone()
                        .unwrap_or_else(|| text_to_html(&row.front)),
                    row.back_html
                        .clone()
                        .unwrap_or_else(|| text_to_html(&row.back)),
                )
            }
        };
        Ok(EditorSeed {
            card_id: card,
            deck_id: row.deck_id,
            note_id: None,
            note_type,
            front_html,
            back_html,
            tags: row.tags,
            sibling_count: 1,
        })
    }

    /// One page of a deck's cards (1-based `page`) plus the total match
    /// count, for the paginated deck view.
    pub fn list_cards_page(
        &self,
        user: UserId,
        deck: DeckId,
        search: Option<&str>,
        page: u32,
        per_page: u32,
    ) -> Result<(Vec<CardRow>, u32)> {
        let per_page = per_page.clamp(1, 500);
        let total = self.store.count_cards(user, Some(deck), search)?;
        let pages = total.div_ceil(per_page).max(1);
        let page = page.clamp(1, pages);
        let rows = self.store.list_cards_page(
            user,
            Some(deck),
            search,
            per_page,
            (page - 1) * per_page,
        )?;
        Ok((rows, total))
    }

    /// Hard-deletes a deck and all its cards; the caller removes the
    /// returned orphan blobs from the media store.
    pub fn delete_deck(&self, user: UserId, deck: DeckId) -> Result<flash_store::DeletedDeck> {
        Ok(self.store.delete_deck(user, deck)?)
    }

    pub fn list_cards(
        &self,
        user: UserId,
        deck: Option<DeckId>,
        search: Option<&str>,
        limit: u32,
    ) -> Result<Vec<CardRow>> {
        let search = bounded_search(search)?;
        Ok(self.store.list_cards(user, deck, search, limit.min(500))?)
    }
}

/// A search term as the store may run it: trimmed, absent when blank,
/// refused past `bounds::SEARCH` (a term longer than that matches
/// nothing a card side can hold, and LIKE's cost grows with it).
fn bounded_search(search: Option<&str>) -> Result<Option<&str>> {
    let search = search.map(str::trim).filter(|q| !q.is_empty());
    if search.is_some_and(|q| q.len() > crate::bounds::SEARCH) {
        return Err(ServiceError::Invalid(format!(
            "search terms are at most {} bytes",
            crate::bounds::SEARCH
        )));
    }
    Ok(search)
}

/// The advanced editor's submission: a note type, two raw HTML fields
/// (sanitized here, never trusted), and the tag list every generated card
/// receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteInput {
    pub note_type: NoteType,
    pub front_html: String,
    pub back_html: String,
    pub tags: Vec<String>,
}

/// What the editor opens with for an existing card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSeed {
    pub card_id: CardId,
    pub deck_id: DeckId,
    /// None: a standalone card the save will adopt into a new note.
    pub note_id: Option<NoteId>,
    pub note_type: NoteType,
    pub front_html: String,
    pub back_html: String,
    pub tags: Vec<String>,
    /// Live cards this save will touch (siblings included).
    pub sibling_count: usize,
}

struct PreparedNote {
    front_html: richtext::SanitizedHtml,
    back_html: richtext::SanitizedHtml,
    generated: Vec<GeneratedCard>,
    tags: Vec<String>,
}

impl PreparedNote {
    fn save(&self, note_type: NoteType, cap: Option<u32>, now_ms: i64) -> NoteSave<'_> {
        NoteSave {
            note_type,
            front_html: &self.front_html,
            back_html: &self.back_html,
            generated: &self.generated,
            tags: &self.tags,
            cap,
            now_ms,
        }
    }
}

/// A stored "more new cards today" boost counts only on the study day it
/// was granted for; any other day it is spent. The one place that rule
/// lives — budgets, boost_today and boost_new_today all read through it.
fn active_boost(boost_new: u32, boost_day: i64, day_start: i64) -> u32 {
    if boost_day == day_start {
        boost_new
    } else {
        0
    }
}

/// Trimmed, lowercased, deduped, empties dropped — the store lowercases
/// too, but doing it here keeps the editor's echo consistent.
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().to_lowercase();
        if !t.is_empty() && seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// Plain card text -> editor HTML: escaped, newlines as `<br>`.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Reconstructs a card's FSRS state by replaying its imported Anki review
/// history (oldest first). Non-chronological entries and invalid ratings
/// are skipped; any scheduler error abandons the replay (card stays new).
/// Replays a card's review history through FSRS. Timestamps are the
/// package's word: one before the epoch or after `now_ms` is skipped,
/// so a crafted revlog cannot leave a card with a "last review" in the
/// far future that every later grade mistakes for a retry.
fn replay_history(scheduler: &Scheduler, reviews: &[(i64, u8)], now_ms: i64) -> Option<CardState> {
    let (first_at, _) = *reviews.iter().find(|(at, _)| (0..=now_ms).contains(at))?;
    let mut state = CardState::new_card(first_at);
    let mut last_at = i64::MIN;
    for &(at, rating) in reviews {
        if at <= last_at || !(0..=now_ms).contains(&at) {
            continue;
        }
        let Some(rating) = Rating::from_i64(rating as i64) else {
            continue;
        };
        match scheduler.review(&state, rating, at) {
            Ok(outcome) => {
                state = outcome.state;
                last_at = at;
            }
            Err(_) => return None,
        }
    }
    (state.reps > 0).then_some(state)
}

/// Current wall-clock in epoch ms.
/// Anki-style typed-answer feedback: the typed string with each char
/// marked right/wrong by LCS alignment against the expected answer (which
/// the revealed back shows anyway). Expected chars the user missed are
/// simply skipped; extra typed chars are wrong; an empty result renders
/// as a single dash.
pub fn type_diff(expected: &str, typed: &str) -> Vec<DiffSpan> {
    let e: Vec<char> = expected.trim().chars().take(200).collect();
    let t: Vec<char> = typed.trim().chars().take(200).collect();
    let (n, m) = (e.len(), t.len());
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if e[i] == t[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut spans: Vec<DiffSpan> = Vec::new();
    let push = |ch: char, ok: bool, spans: &mut Vec<DiffSpan>| match spans.last_mut() {
        Some(s) if s.ok == ok => s.text.push(ch),
        _ => spans.push(DiffSpan {
            text: ch.to_string(),
            ok,
        }),
    };
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if e[i] == t[j] {
            push(t[j], true, &mut spans);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1; // expected char the user missed; the back shows it
        } else {
            push(t[j], false, &mut spans);
            j += 1;
        }
    }
    while j < m {
        push(t[j], false, &mut spans);
        j += 1;
    }
    if spans.is_empty() {
        spans.push(DiffSpan {
            text: "—".into(),
            ok: false,
        });
    }
    spans
}

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}
