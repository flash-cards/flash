//! MCP server: the voice-study surface. Tool descriptions and server
//! instructions carry the conversation protocol; responses stay terse so
//! there is nothing scheduling-ish for a voice model to read aloud.

use flash_core::queue::StudyScope;
use flash_core::{CardId, DeckId, GradingMode, Rating, SessionId, UserId};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, Json};

use std::sync::Arc;

use crate::ext::{ServerExtension, Surface};
use crate::oauth::ConnectorClient;
use crate::service::{now_ms, ReviewOrigin, ServiceError, Services};

// Kept to two sentences on purpose: claude.ai discards server-level
// instructions entirely (only per-tool descriptions reach the model), so
// every rule that matters rides in the tool descriptions and responses.
const INSTRUCTIONS: &str = "\
Spaced-repetition flashcard server. Whenever the user asks to study, \
review, practice, quiz, test, or drill their cards, call \
start_study_session and follow the tool descriptions and responses — \
never quiz from conversation history, because only submit_review records \
progress and drives the schedule.";

/// The largest JSON-RPC message MCP accepts; a card's two fields are a
/// few tens of kilobytes at most, and `create_cards` takes at most
/// `bounds::CARDS_PER_CALL` of them.
pub const MCP_BODY_LIMIT: usize = 1024 * 1024;

/// `public_host` is the authority of FLASH_BASE_URL: rmcp validates the
/// Host header against loopback only by default (DNS-rebinding guard),
/// which would reject requests arriving through a proxy or tunnel.
pub fn mcp_service(
    services: Services,
    media: std::sync::Arc<dyn crate::media_store::MediaStore>,
    ext: Arc<dyn ServerExtension>,
    public_host: String,
) -> StreamableHttpService<FlashMcp, LocalSessionManager> {
    let mut config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
    config.allowed_hosts = vec![
        public_host,
        "localhost:8437".to_string(),
        "127.0.0.1:8437".to_string(),
    ];
    // The transport reads the body itself, so this is the only limit
    // that applies to it (an axum body-limit layer around the service is
    // a marker its extractors consult and this service never does).
    config.max_request_body_bytes = MCP_BODY_LIMIT;
    StreamableHttpService::new(
        move || Ok(FlashMcp::new(services.clone(), media.clone(), ext.clone())),
        Default::default(),
        config,
    )
}

#[derive(Clone)]
pub struct FlashMcp {
    services: Services,
    /// Blob storage, for orphan cleanup after deck deletion.
    media: std::sync::Arc<dyn crate::media_store::MediaStore>,
    /// The deployment's per-account counters.
    ext: Arc<dyn ServerExtension>,
    /// Deletions asked for but not yet confirmed, keyed by the token the
    /// first call handed back. Per MCP session: a token minted here is
    /// good only here, for one target, for a minute.
    pending_deletes: Arc<parking_lot::Mutex<std::collections::HashMap<String, PendingDelete>>>,
    // #[tool_handler] resolves the router via Self::tool_router(); the
    // field exists to satisfy the #[tool_router] macro's constructor shape.
    #[allow(dead_code)]
    tool_router: ToolRouter<FlashMcp>,
}

fn authed_user(ctx: &RequestContext<RoleServer>) -> Result<UserId, McpError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<UserId>().copied())
        .ok_or_else(|| McpError::invalid_request("unauthenticated", None))
}

/// The registered name of the client making the call, when the bearer
/// layer resolved one.
fn authed_client(ctx: &RequestContext<RoleServer>) -> Option<String> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<ConnectorClient>())
        .map(|c| c.0.clone())
        .filter(|name| !name.is_empty())
}

fn to_mcp_err(err: ServiceError) -> McpError {
    match err {
        ServiceError::Invalid(msg) => McpError::invalid_params(msg, None),
        ServiceError::Store(flash_store::StoreError::NotFound(what)) => {
            McpError::invalid_params(format!("not found: {what}"), None)
        }
        ServiceError::OverCap { .. } => McpError::invalid_params(err.to_string(), None),
        other => {
            tracing::error!("mcp service error: {other:?}");
            McpError::internal_error("internal error", None)
        }
    }
}

async fn run<T, F>(services: &Services, f: F) -> Result<T, McpError>
where
    T: Send + 'static,
    F: FnOnce(Services) -> Result<T, ServiceError> + Send + 'static,
{
    let services = services.clone();
    tokio::task::spawn_blocking(move || f(services))
        .await
        .map_err(|e| {
            tracing::error!("join: {e}");
            McpError::internal_error("internal error", None)
        })?
        .map_err(to_mcp_err)
}

/// Plain-language guidance for the model when the queue is empty, so it
/// neither declares the user "done" when a daily limit is the real reason
/// nor falls back to quizzing from chat.
fn queue_note(in_queue: u32, held_by_limit: u32) -> Option<String> {
    if in_queue > 0 {
        return None;
    }
    Some(if held_by_limit > 0 {
        format!(
            "Queue empty for now, but {held_by_limit} more card(s) are held back by \
             today's daily limit (new cards per day, default 20). Tell the user \
             they've finished today's allowance and that {held_by_limit} remain; if \
             they want to keep going, call boost_new_today (with this deck's name \
             if the session was deck-scoped) and then start_study_session again. \
             Do not quiz from chat."
        )
    } else {
        "Nothing to study right now: every card in scope comes back later. \
         Tell the user they're caught up. Do not quiz from chat."
            .to_string()
    })
}

fn grading_instruction(mode: GradingMode) -> &'static str {
    match mode {
        GradingMode::Silent => {
            "silent: never mention ratings, intervals, or scheduling; confirm \
             correctness naturally and continue"
        }
        GradingMode::Announce => {
            "announce: state your rating in one word after each answer; if the \
             user corrects it, resubmit with their rating"
        }
        GradingMode::SelfGrade => {
            "self: after revealing whether they were right, ask the user to \
             rate the card 1-4 and submit their rating"
        }
    }
}

// ---- tool output types. Returning Json<T> makes the #[tool] macro derive
// each tool's outputSchema from these via schemars, and rmcp emits the
// payload as structuredContent plus the same JSON as a text block (so
// clients without structured-content support see exactly what they did
// before). Doc comments become schema descriptions.

/// One card front for the assistant to ask.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardFrontOut {
    pub card_id: i64,
    /// The question side; ask it and wait for the user's answer.
    pub front: String,
}

fn card_out(card: &crate::service::CardFront) -> CardFrontOut {
    CardFrontOut {
        card_id: card.card_id.0,
        front: card.front.clone(),
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SessionStartOut {
    pub session_id: i64,
    /// Cards in this session's queue right now.
    pub cards_due: u32,
    /// How to report grades for the whole session; follow it exactly.
    pub grading_mode: String,
    /// First card to ask, or null when nothing is due.
    pub first_card: Option<CardFrontOut>,
    /// Present only when the queue is empty: why, and what to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SubmitOut {
    pub recorded: bool,
    /// Next card to ask immediately, or null when the session is finished.
    pub next_card: Option<CardFrontOut>,
    /// Cards left in the queue.
    pub remaining: u32,
    /// Present only when the queue is empty: why, and what to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct AnswerOut {
    /// The card's back (the answer).
    pub back: String,
}

/// Session summary: total reviewed and counts by rating.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SessionSummaryOut {
    pub reviewed: u32,
    pub again: u32,
    pub hard: u32,
    pub good: u32,
    pub easy: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DeckOut {
    pub name: String,
    /// Reviews due now.
    pub due: u32,
    /// New cards available today.
    pub r#new: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DecksOut {
    pub decks: Vec<DeckOut>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardOut {
    pub card_id: i64,
    pub front: String,
    pub back: String,
    pub tags: Vec<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardsOut {
    pub cards: Vec<CardOut>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct AccountLimitsOut {
    pub new_per_day: u32,
    pub reviews_per_day: u32,
    /// Extra new cards granted for today.
    pub boost_today: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DeckLimitsOut {
    pub name: String,
    /// Deck override; null = inherits the account default.
    pub new_per_day: Option<u32>,
    /// Deck override; null = inherits the account default.
    pub reviews_per_day: Option<u32>,
    /// Extra new cards granted to this deck for today.
    pub boost_today: u32,
    /// New cards this deck can still serve today.
    pub new_available: u32,
    /// Reviews due now in this deck.
    pub due: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LimitsOut {
    pub account: AccountLimitsOut,
    pub decks: Vec<DeckLimitsOut>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct BoostOut {
    /// Total extra new cards granted today (after this call).
    pub boost_today: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CreatedOut {
    pub created: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardsCreatedOut {
    /// Number of cards created.
    pub created: u32,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct UpdatedOut {
    pub updated: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DeleteOutcome {
    /// True only after a confirmed call removed the target.
    pub deleted: bool,
    /// True when this call recorded the request and is waiting for the
    /// person's yes; `confirm_token` is set and nothing was removed.
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_token: Option<String>,
    /// Cards this deletion removes (removed, once `deleted` is true).
    pub cards_affected: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What a pending deletion names; a token spends only on the same target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    Card(CardId),
    Deck(DeckId),
}

pub struct PendingDelete {
    pub user: UserId,
    pub target: DeleteTarget,
    pub expires_ms: i64,
}

/// How long a confirm token lives: long enough to ask a person, short
/// enough that a token quoted in card text has gone stale by the time
/// it could be replayed.
const CONFIRM_TTL_MS: i64 = 60_000;
/// Pending deletions kept per session; past this the oldest are shed.
const PENDING_DELETES_CAP: usize = 32;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteCardArgs {
    pub card_id: i64,
    /// Omit on the first call; pass back the token the first call returned,
    /// after the user has confirmed.
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    pub confirm_token: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteDeckArgs {
    /// Deck name.
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub deck: String,
    /// Omit on the first call; pass back the token the first call returned,
    /// after the user has confirmed.
    #[serde(default, deserialize_with = "crate::bounds::opt_token")]
    pub confirm_token: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SavedOut {
    pub saved: bool,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct StartSessionArgs {
    /// Deck name to study (omit to study everything due).
    #[serde(default, deserialize_with = "crate::bounds::opt_deck_name")]
    pub deck: Option<String>,
    /// Tag to study, e.g. "exam-2" (ignored when deck is set).
    #[serde(default, deserialize_with = "crate::bounds::opt_tag")]
    pub tag: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SubmitReviewArgs {
    pub session_id: i64,
    pub card_id: i64,
    /// 1=Again 2=Hard 3=Good 4=Easy
    pub rating: i64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CardIdArgs {
    pub card_id: i64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SessionIdArgs {
    pub session_id: i64,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateDeckArgs {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub name: String,
    #[serde(default, deserialize_with = "crate::bounds::opt_deck_description")]
    pub description: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeckNameArgs {
    /// Deck name.
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub deck: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct NewCard {
    #[serde(deserialize_with = "crate::bounds::card_side")]
    pub front: String,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    pub back: String,
    /// Optional tags, e.g. ["exam-2", "cardiac"]
    #[serde(default, deserialize_with = "crate::bounds::opt_tags")]
    pub tags: Option<Vec<String>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateCardsArgs {
    /// Target deck; created if it doesn't exist.
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub deck: String,
    #[serde(deserialize_with = "crate::bounds::cards")]
    pub cards: Vec<NewCard>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateCardArgs {
    pub card_id: i64,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    pub front: String,
    #[serde(deserialize_with = "crate::bounds::card_side")]
    pub back: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListCardsArgs {
    /// Filter to one deck by name.
    #[serde(default, deserialize_with = "crate::bounds::opt_deck_name")]
    pub deck: Option<String>,
    /// Substring search over fronts and backs.
    #[serde(default, deserialize_with = "crate::bounds::opt_search")]
    pub search: Option<String>,
    pub limit: Option<u32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetLimitsArgs {
    /// Deck name to set an override for; omit to set the account defaults.
    #[serde(default, deserialize_with = "crate::bounds::opt_deck_name")]
    pub deck: Option<String>,
    /// New cards per day. Omit to leave unchanged; for a deck, -1 clears
    /// the override so it inherits the account default.
    pub new_per_day: Option<i64>,
    /// Max reviews per day. Omit to leave unchanged; for a deck, -1 clears
    /// the override so it inherits the account default.
    pub reviews_per_day: Option<i64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct BoostArgs {
    /// Deck name; omit to boost the account-wide allowance.
    #[serde(default, deserialize_with = "crate::bounds::opt_deck_name")]
    pub deck: Option<String>,
    /// Extra new cards to allow today (1-500). Today only.
    pub extra: u32,
}

#[tool_router]
impl FlashMcp {
    pub fn new(
        services: Services,
        media: std::sync::Arc<dyn crate::media_store::MediaStore>,
        ext: Arc<dyn ServerExtension>,
    ) -> Self {
        Self {
            services,
            media,
            ext,
            pending_deletes: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// The first half of a destructive tool call: records what the
    /// caller asked to delete and hands back a token that only a second
    /// call, in this session, for this target, within a minute, can
    /// spend. The model has to come back after the person said yes; a
    /// single call, however it was prompted, deletes nothing.
    fn ask_to_confirm(&self, user: UserId, target: DeleteTarget, cards: u32) -> DeleteOutcome {
        let token = crate::auth::new_token();
        let mut pending = self.pending_deletes.lock();
        let now = now_ms();
        pending.retain(|_, p| p.expires_ms > now);
        if pending.len() >= PENDING_DELETES_CAP {
            pending.clear();
        }
        pending.insert(
            token.clone(),
            PendingDelete {
                user,
                target,
                expires_ms: now + CONFIRM_TTL_MS,
            },
        );
        DeleteOutcome {
            deleted: false,
            pending: true,
            confirm_token: Some(token),
            cards_affected: cards,
            note: Some(
                "Nothing was deleted. Tell the user exactly what this would remove and \
                 ask them to confirm. Only if they say yes, call this tool again with \
                 the same arguments plus this confirm_token. The token expires in one \
                 minute and works once."
                    .to_string(),
            ),
        }
    }

    /// The second half: the token must exist, be the caller's, name this
    /// target, and be fresh. A match spends it (whether or not the
    /// deletion then succeeds); a mismatch leaves it for the call it was
    /// minted for, so a stray call cannot burn a person's real yes.
    fn confirmed(&self, user: UserId, target: &DeleteTarget, token: &str) -> Result<(), McpError> {
        let mut pending = self.pending_deletes.lock();
        let now = now_ms();
        let matches = pending
            .get(token)
            .is_some_and(|p| p.user == user && &p.target == target && p.expires_ms > now);
        if matches {
            pending.remove(token);
            return Ok(());
        }
        pending.retain(|_, p| p.expires_ms > now);
        Err(McpError::invalid_params(
            "confirm_token is unknown, expired, or for a different target; call \
             again without one to be asked afresh",
            None,
        ))
    }

    /// Counts something the caller's account did over MCP, labelled with
    /// the client's registered name.
    fn account_event(
        &self,
        ctx: &RequestContext<RoleServer>,
        user: UserId,
        event: &'static str,
        amount: Option<i64>,
    ) {
        let client = authed_client(ctx);
        self.ext.account_event(
            &self.services,
            user,
            event,
            Surface::Mcp,
            client.as_deref(),
            amount,
        );
    }

    fn resolve_scope(
        services: &Services,
        user: UserId,
        deck: Option<String>,
        tag: Option<String>,
    ) -> Result<StudyScope, ServiceError> {
        match (deck, tag) {
            (Some(name), _) => {
                let id = services
                    .store()
                    .find_deck_by_name(user, &name)?
                    .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))?;
                Ok(StudyScope::Deck(id))
            }
            (None, Some(tag)) => Ok(StudyScope::Tag(tag)),
            _ => Ok(StudyScope::All),
        }
    }

    #[tool(
        description = "Start a study session. REQUIRED whenever the user asks \
        to study, review, practice, quiz, test, or drill their cards — even \
        cards created moments ago in this chat. Do not quiz from memory: \
        only sessions record progress. Returns the first card's front \
        (ask it aloud and wait for the user's answer), how many cards are \
        due, and grading_mode — follow its instruction exactly for the whole \
        session. Internal fields (ids, counts) are never read aloud.",
        annotations(
            title = "Start study session",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn start_study_session(
        &self,
        Parameters(args): Parameters<StartSessionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SessionStartOut>, McpError> {
        let user = authed_user(&ctx)?;
        let session = run(&self.services, move |s| {
            let scope = Self::resolve_scope(&s, user, args.deck, args.tag)?;
            s.start_session(user, scope, now_ms())
        })
        .await?;
        Ok(Json(SessionStartOut {
            session_id: session.session_id.0,
            cards_due: session.cards_due,
            grading_mode: grading_instruction(session.grading_mode).to_string(),
            first_card: session.first_card.as_ref().map(card_out),
            note: queue_note(session.cards_due, session.held_by_limit),
        }))
    }

    #[tool(
        description = "Record the user's result on a card and get the next \
        one. Grade STRICTLY against the card's back: facts, numbers, doses, \
        and units must match precisely — an imprecise answer is 1 (Again), \
        never 'close enough'; only phrasing may differ. 1=Again (wrong or \
        blank), 2=Hard (incomplete or hesitant), 3=Good (fully correct), \
        4=Easy (instant, perfect). State the exact answer on a miss, then \
        immediately ask next_card's front. A null next_card ends the \
        session — wrap up briefly.",
        annotations(
            title = "Submit review",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn submit_review(
        &self,
        Parameters(args): Parameters<SubmitReviewArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SubmitOut>, McpError> {
        let user = authed_user(&ctx)?;
        let client = authed_client(&ctx);
        let rating = Rating::from_i64(args.rating)
            .ok_or_else(|| McpError::invalid_params("rating must be 1-4", None))?;
        let result = run(&self.services, move |s| {
            s.submit_review(
                user,
                SessionId(args.session_id),
                CardId(args.card_id),
                rating,
                ReviewOrigin::new("mcp", client.as_deref()),
                now_ms(),
            )
        })
        .await?;
        Ok(Json(SubmitOut {
            recorded: true,
            next_card: result.next_card.as_ref().map(card_out),
            remaining: result.remaining,
            note: queue_note(result.remaining, result.held_by_limit),
        }))
    }

    #[tool(
        description = "Get a card's back (the answer). Use when the user \
        gives up or asks for the answer: read it aloud, then call \
        submit_review with rating 1.",
        annotations(
            title = "Show answer",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn show_answer(
        &self,
        Parameters(args): Parameters<CardIdArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<AnswerOut>, McpError> {
        let user = authed_user(&ctx)?;
        let back = run(&self.services, move |s| {
            s.reveal(user, CardId(args.card_id))
        })
        .await?;
        Ok(Json(AnswerOut { back }))
    }

    #[tool(
        description = "End a study session and get its summary (counts by \
        rating and total reviewed). Give the user a one-sentence recap.",
        annotations(
            title = "End session",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn end_session(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SessionSummaryOut>, McpError> {
        let user = authed_user(&ctx)?;
        let stats = run(&self.services, move |s| {
            s.end_session(user, SessionId(args.session_id), now_ms())
        })
        .await?;
        Ok(Json(SessionSummaryOut {
            reviewed: stats.reviewed,
            again: stats.again,
            hard: stats.hard,
            good: stats.good,
            easy: stats.easy,
        }))
    }

    #[tool(
        description = "List the user's decks with due and new card counts.",
        annotations(
            title = "List decks",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_decks(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<DecksOut>, McpError> {
        let user = authed_user(&ctx)?;
        let decks = run(&self.services, move |s| s.list_decks(user, now_ms())).await?;
        let rows = decks
            .iter()
            .map(|d| DeckOut {
                name: d.name.clone(),
                due: d.due_count,
                r#new: d.new_count,
            })
            .collect();
        Ok(Json(DecksOut { decks: rows }))
    }

    #[tool(
        description = "Create a new (empty) deck.",
        annotations(
            title = "Create deck",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_deck(
        &self,
        Parameters(args): Parameters<CreateDeckArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<CreatedOut>, McpError> {
        let user = authed_user(&ctx)?;
        run(&self.services, move |s| {
            s.create_deck(
                user,
                &args.name,
                args.description.as_deref().unwrap_or(""),
                now_ms(),
            )
        })
        .await?;
        self.account_event(&ctx, user, "deck_created", None);
        Ok(Json(CreatedOut { created: true }))
    }

    #[tool(
        description = "Create flashcards in a deck (created if missing). \
        Ideal for turning pasted notes into cards: batch all cards into one \
        call. Keep fronts a single clear question and backs a concise \
        answer — cards are asked one at a time in study sessions.",
        annotations(
            title = "Create cards",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_cards(
        &self,
        Parameters(args): Parameters<CreateCardsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<CardsCreatedOut>, McpError> {
        let user = authed_user(&ctx)?;
        let created = run(&self.services, move |s| {
            let cards: Vec<(String, String, Vec<String>)> = args
                .cards
                .into_iter()
                .map(|c| (c.front, c.back, c.tags.unwrap_or_default()))
                .collect();
            s.create_cards(user, &args.deck, &cards, now_ms())
        })
        .await?;
        self.account_event(&ctx, user, "cards_created", Some(created.len() as i64));
        Ok(Json(CardsCreatedOut {
            created: created.len() as u32,
        }))
    }

    #[tool(
        description = "Rewrite a card's front and back text.",
        annotations(
            title = "Update card",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn update_card(
        &self,
        Parameters(args): Parameters<UpdateCardArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<UpdatedOut>, McpError> {
        let user = authed_user(&ctx)?;
        run(&self.services, move |s| {
            s.update_card(
                user,
                CardId(args.card_id),
                &args.front,
                &args.back,
                now_ms(),
            )
        })
        .await?;
        self.account_event(&ctx, user, "card_edited", None);
        Ok(Json(UpdatedOut { updated: true }))
    }

    #[tool(
        description = "Delete a card. Two steps: the first call deletes nothing and \
        returns a confirm_token; ask the user, and only if they say yes call again \
        with the same card_id and that confirm_token.",
        annotations(
            title = "Delete card",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_card(
        &self,
        Parameters(args): Parameters<DeleteCardArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<DeleteOutcome>, McpError> {
        let user = authed_user(&ctx)?;
        let card = CardId(args.card_id);
        // The card must be the caller's before either step says anything.
        run(&self.services, move |s| s.card(user, card)).await?;
        let target = DeleteTarget::Card(card);
        let Some(token) = args.confirm_token else {
            return Ok(Json(self.ask_to_confirm(user, target, 1)));
        };
        self.confirmed(user, &target, &token)?;
        run(&self.services, move |s| s.delete_card(user, card, now_ms())).await?;
        Ok(Json(DeleteOutcome {
            deleted: true,
            pending: false,
            confirm_token: None,
            cards_affected: 1,
            note: None,
        }))
    }

    #[tool(
        description = "Delete a deck and every card in it, permanently. Two steps: \
        the first call deletes nothing and returns a confirm_token with the card \
        count; ask the user, and only if they say yes call again with the same deck \
        and that confirm_token.",
        annotations(
            title = "Delete deck",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_deck(
        &self,
        Parameters(args): Parameters<DeleteDeckArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<DeleteOutcome>, McpError> {
        let user = authed_user(&ctx)?;
        let name = args.deck.clone();
        let (id, cards) = run(&self.services, move |s| {
            let id = s
                .store()
                .find_deck_by_name(user, &name)?
                .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))?;
            // The number the person is asked to approve is the real
            // one, not a page of it.
            let cards = s.store().count_cards(user, Some(id), None)?;
            Ok((id, cards))
        })
        .await?;
        let target = DeleteTarget::Deck(id);
        let Some(token) = args.confirm_token else {
            return Ok(Json(self.ask_to_confirm(user, target, cards)));
        };
        self.confirmed(user, &target, &token)?;
        let deleted = run(&self.services, move |s| s.delete_deck(user, id)).await?;
        crate::media::remove_orphans(
            self.services.clone(),
            self.media.clone(),
            &deleted.orphan_blobs,
        );
        Ok(Json(DeleteOutcome {
            deleted: true,
            pending: false,
            confirm_token: None,
            cards_affected: deleted.cards,
            note: None,
        }))
    }

    #[tool(
        description = "List cards, optionally filtered by deck name or a search term.",
        annotations(
            title = "List cards",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_cards(
        &self,
        Parameters(args): Parameters<ListCardsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<CardsOut>, McpError> {
        let user = authed_user(&ctx)?;
        let rows = run(&self.services, move |s| {
            let deck = match args.deck {
                Some(name) => Some(
                    s.store()
                        .find_deck_by_name(user, &name)?
                        .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))?,
                ),
                None => None,
            };
            s.list_cards(user, deck, args.search.as_deref(), args.limit.unwrap_or(50))
        })
        .await?;
        let cards = rows
            .iter()
            .map(|c| CardOut {
                card_id: c.id.0,
                front: c.front.clone(),
                back: c.back.clone(),
                tags: c.tags.clone(),
            })
            .collect();
        Ok(Json(CardsOut { cards }))
    }

    #[tool(
        description = "Show today's study limits: the account defaults (new \
        cards per day, max reviews per day, any extra new cards granted \
        today) and every deck's override (null = inherits) with how many new \
        and due cards it can still serve today.",
        annotations(
            title = "Get limits",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_limits(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<LimitsOut>, McpError> {
        let user = authed_user(&ctx)?;
        let (settings, boost, rows) = run(&self.services, move |s| {
            let now = now_ms();
            let settings = s.store().get_settings(user)?;
            let boost = s.boost_today(user, None, now)?;
            let mut rows = Vec::new();
            for d in s.list_decks(user, now)? {
                let l = s.deck_limits(user, d.id)?;
                let b = s.boost_today(user, Some(d.id), now)?;
                rows.push(DeckLimitsOut {
                    name: d.name,
                    new_per_day: l.new_per_day,
                    reviews_per_day: l.reviews_per_day,
                    boost_today: b,
                    new_available: d.new_count,
                    due: d.due_count,
                });
            }
            Ok((settings, boost, rows))
        })
        .await?;
        Ok(Json(LimitsOut {
            account: AccountLimitsOut {
                new_per_day: settings.new_per_day,
                reviews_per_day: settings.reviews_per_day,
                boost_today: boost,
            },
            decks: rows,
        }))
    }

    #[tool(
        description = "Change daily study limits. Without a deck: sets the \
        account defaults. With a deck: sets that deck's override (-1 clears \
        it so the deck inherits the account default). Omitted fields are \
        left unchanged.",
        annotations(
            title = "Set limits",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_limits(
        &self,
        Parameters(args): Parameters<SetLimitsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SavedOut>, McpError> {
        let user = authed_user(&ctx)?;
        for v in [args.new_per_day, args.reviews_per_day]
            .into_iter()
            .flatten()
        {
            if v < -1 {
                return Err(McpError::invalid_params(
                    "limits must be >= 0 (or -1 to clear)",
                    None,
                ));
            }
        }
        run(&self.services, move |s| match args.deck {
            None => {
                let current = s.store().get_settings(user)?;
                let pick = |v: Option<i64>, cur: u32| match v {
                    Some(n) if n >= 0 => n as u32,
                    _ => cur,
                };
                s.set_daily_limits(
                    user,
                    pick(args.new_per_day, current.new_per_day),
                    pick(args.reviews_per_day, current.reviews_per_day),
                )
            }
            Some(name) => {
                let id = s
                    .store()
                    .find_deck_by_name(user, &name)?
                    .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))?;
                let current = s.deck_limits(user, id)?;
                let pick = |v: Option<i64>, cur: Option<u32>| match v {
                    None => cur,
                    Some(-1) => None,
                    Some(n) => Some(n as u32),
                };
                s.set_deck_limits(
                    user,
                    id,
                    pick(args.new_per_day, current.new_per_day),
                    pick(args.reviews_per_day, current.reviews_per_day),
                )
            }
        })
        .await?;
        Ok(Json(SavedOut { saved: true }))
    }

    #[tool(
        description = "Allow extra new cards for today only ('give me 10 more \
        new cards today'). With a deck name it applies to that deck; without \
        one, to the account-wide allowance. Returns the total extra granted \
        today.",
        annotations(
            title = "More new cards today",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn boost_new_today(
        &self,
        Parameters(args): Parameters<BoostArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<BoostOut>, McpError> {
        let user = authed_user(&ctx)?;
        let total = run(&self.services, move |s| {
            let deck = match args.deck {
                Some(name) => Some(
                    s.store()
                        .find_deck_by_name(user, &name)?
                        .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))?,
                ),
                None => None,
            };
            s.boost_new_today(user, deck, args.extra, now_ms())
        })
        .await?;
        Ok(Json(BoostOut { boost_today: total }))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for FlashMcp {
    fn get_info(&self) -> ServerInfo {
        use base64::Engine;
        let mut info = ServerInfo::default();
        info.server_info.name = "flash".to_string();
        info.server_info.title = Some("Flash".to_string());
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info.description =
            Some("Spaced-repetition flashcards for voice study.".to_string());
        // SEP-973 server icons (MCP spec 2025-11-25): a data URI keeps the
        // metadata self-contained. The favicon is a full-bleed square on
        // purpose: hosts clip icons with their own corner radius, and a tile
        // with baked-in rounding shows its transparent corners as white wedges.
        let icon_uri = format!(
            "data:image/svg+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(include_str!("../static/favicon.svg"))
        );
        info.server_info.icons = Some(vec![rmcp::model::Icon::new(icon_uri)
            .with_mime_type("image/svg+xml")
            .with_sizes(vec!["any".to_string()])]);
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

#[cfg(test)]
mod icon_tests {
    /// Connector dialogs and app launchers round icons themselves; a tile
    /// with its own radius shows white wedges in the corners.
    #[test]
    fn favicon_is_a_full_bleed_square() {
        let svg = include_str!("../static/favicon.svg");
        assert!(svg.starts_with("<svg"));
        assert!(!svg.contains("rx="), "corner radius crept back: {svg}");
        assert!(svg.contains("<rect width=\"64\" height=\"64\" fill="));
    }
}
