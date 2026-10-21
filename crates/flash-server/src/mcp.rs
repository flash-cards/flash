//! MCP server: rich card management and study. Tool descriptions and server
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

fn rich_side(html: Option<&str>, plain: &str) -> String {
    html.map(str::to_string)
        .unwrap_or_else(|| crate::service::text_to_html(plain))
}

fn media_out(
    s: &Services,
    user: UserId,
    base: &str,
    fields: &[&str],
) -> Result<Vec<MediaOut>, ServiceError> {
    let mut ids = std::collections::BTreeSet::new();
    for field in fields {
        ids.extend(flash_store::richtext::media_ids(field));
    }
    ids.into_iter()
        .map(|id| {
            let row = s
                .store()
                .get_media(user, flash_core::MediaId(id))?
                .ok_or_else(|| ServiceError::Invalid(format!("missing media reference {id}")))?;
            Ok(MediaOut {
                media_id: id,
                url: format!("{}/mcp-media/{id}", base.trim_end_matches('/')),
                mime: row.mime,
                filename: row.filename,
                size: row.size,
            })
        })
        .collect()
}

fn mutation_out(saved: flash_store::McpMutationReceipt) -> MutationOut {
    MutationOut {
        source_ids: saved.sources.iter().map(|(id, _)| id.0).collect(),
        card_ids: saved
            .sources
            .iter()
            .flat_map(|(_, ids)| ids.iter().map(|id| id.0))
            .collect(),
        created: saved.created,
        updated: saved.updated,
        deck_totals: saved.deck_totals,
    }
}

// Kept to two sentences on purpose: claude.ai discards server-level
// instructions entirely (only per-tool descriptions reach the model), so
// every rule that matters rides in the tool descriptions and responses.
const INSTRUCTIONS: &str = "\
Spaced-repetition flashcard server. Whenever the user asks to study, \
review, practice, quiz, test, or drill their cards, call \
start_study_session and follow the tool descriptions and responses, and \
never quiz from conversation history, because only submit_review records \
progress and drives the schedule.";

/// The largest JSON-RPC message MCP accepts; a card's two fields are a
/// few tens of kilobytes at most, and `create_cards` takes at most
/// `bounds::CARDS_PER_CALL` of them.
pub const MCP_BODY_LIMIT: usize = 1024 * 1024;

/// The most card JSON one `list_cards` page carries. The result travels
/// both as text and as structured content, and clients cap what a tool
/// may return (commonly some 25,000 tokens of text), so a page stops well
/// short of that and the cursor carries on.
const LIST_PAGE_BYTES: usize = 64 * 1024;

/// The `host[:port]` a request through `base_url` carries in its Host
/// header. Parsed as a URL so a trailing slash or path on the base URL
/// does not leak into the allowlist entry.
pub fn public_host(base_url: &str) -> String {
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| "localhost:8437".to_string())
}

/// `public_host` is the authority of FLASH_BASE_URL: rmcp validates the
/// Host header against loopback only by default (DNS-rebinding guard),
/// which would reject requests arriving through a proxy or tunnel.
pub fn mcp_service(
    services: Services,
    media: std::sync::Arc<dyn crate::media_store::MediaStore>,
    ext: Arc<dyn ServerExtension>,
    public_host: String,
    base_url: String,
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
        move || {
            Ok(FlashMcp::new(
                services.clone(),
                media.clone(),
                ext.clone(),
                base_url.clone(),
            ))
        },
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
    base_url: String,
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

/// What a refused deletion confirmation means and what to do next. A bare
/// code left the model guessing whether its own call had deleted anything.
/// In every case this call deleted nothing.
fn confirmation_hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "confirmation_target_changed" => {
            "the cards, the deck or the subdecks under it changed (a subdeck added, moved or removed counts), or were already deleted (perhaps from another chat or the app), after the preview. This call deleted nothing. Read the current state; if something still needs deleting, call again without confirm_token and ask the user once more."
        }
        "confirmation_expired" => {
            "the ten minutes passed. This call deleted nothing. Call again without confirm_token for a fresh preview and ask the user once more."
        }
        "confirmation_target_mismatch" => {
            "this token was issued for a different deck or set of card IDs. This call deleted nothing. Repeat exactly the arguments of the call that returned the token, or start over without confirm_token."
        }
        "confirmation_invalid" => {
            "this token is unknown to this account and connector. This call deleted nothing. Call again without confirm_token for a fresh preview."
        }
        "too_many_pending_confirmations" => {
            "too many deletions are waiting for an answer. Finish or let lapse the ones already asked (ten minutes) before staging more."
        }
        _ => return None,
    })
}

fn to_mcp_err(err: ServiceError) -> McpError {
    match err {
        ServiceError::Invalid(msg) => McpError::invalid_params(msg, None),
        ServiceError::Store(flash_store::StoreError::Invalid(msg)) => {
            let code = msg.split(':').next().unwrap_or("invalid").to_string();
            tracing::warn!(error_code=%code, "MCP operation rejected");
            let msg = match confirmation_hint(&code) {
                Some(hint) => format!("{code}: {hint}"),
                None => msg,
            };
            McpError::invalid_params(msg, Some(serde_json::json!({"code":code})))
        }
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

/// The user's deck by name, or the error every deck-naming tool shares.
fn deck_named(s: &Services, user: UserId, name: &str) -> Result<DeckId, ServiceError> {
    s.store()
        .find_deck_by_name(user, name)?
        .ok_or_else(|| ServiceError::Invalid(format!("no deck named '{name}'")))
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

/// Rich question only. Editable source is deliberately absent during study.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardFrontOut {
    pub card_id: i64,
    pub front_html: String,
    pub media: Vec<MediaOut>,
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
    pub back_html: String,
    pub media: Vec<MediaOut>,
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
    /// Archived: kept whole but out of study; its counts are zero.
    pub archived: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DecksOut {
    pub decks: Vec<DeckOut>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct MediaOut {
    pub media_id: i64,
    pub url: String,
    pub mime: String,
    pub filename: String,
    pub size: u64,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct SourceOut {
    /// Non-null when a legacy import lacks a lossless editable source.
    pub read_only_reason: Option<String>,
    pub note_id: Option<i64>,
    pub note_type: String,
    pub front_html: String,
    pub back_html: String,
    pub tags: Vec<String>,
    pub sibling_card_ids: Vec<i64>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardOut {
    pub card_id: i64,
    pub deck_id: i64,
    pub ord: u32,
    pub cloze_index: Option<u32>,
    pub type_answer: Option<String>,
    pub front_html: String,
    pub back_html: String,
    pub source: SourceOut,
    pub media: Vec<MediaOut>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct CardsOut {
    pub cards: Vec<CardOut>,
    pub total_count: u32,
    pub next_cursor: Option<i64>,
    pub has_more: bool,
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
pub struct MutationOut {
    pub source_ids: Vec<i64>,
    pub card_ids: Vec<i64>,
    pub created: u32,
    pub updated: u32,
    pub deck_totals: Vec<(i64, u32)>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct DeleteOutcome {
    pub deleted: bool,
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_token: Option<String>,
    pub cards_affected: u32,
    pub card_ids: Vec<i64>,
    /// A deck deletion: the decks that go, by name, the deck first and
    /// then the decks under it. Empty for a deletion of cards.
    pub decks_affected: Vec<String>,
    pub deck_totals: Vec<(i64, u32)>,
    pub expires_ms: Option<i64>,
    pub note: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteCardsArgs {
    #[serde(deserialize_with = "crate::bounds::cards")]
    pub card_ids: Vec<i64>,
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
    /// Deck name to study (omit to study everything due). A deck includes
    /// its subdecks; name Parent::Child to study one subdeck.
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
#[serde(deny_unknown_fields)]
pub struct RenameDeckArgs {
    /// The deck's current full name, e.g. "Cardio" or "Pharm::Cardio".
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub deck: String,
    /// Its new full name. "Pharm::Cardio" puts it inside Pharm; a name
    /// with no "::" puts it at the top level.
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub new_name: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct RenamedOut {
    pub renamed: bool,
    /// The deck's full name now.
    pub name: String,
    /// Every deck that has a new name: the deck and its subdecks.
    pub decks_renamed: u32,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCard {
    #[serde(default, deserialize_with = "crate::bounds::opt_keyword")]
    pub note_type: Option<String>,
    #[serde(deserialize_with = "crate::bounds::field_html")]
    pub front_html: String,
    #[serde(deserialize_with = "crate::bounds::field_html")]
    pub back_html: String,
    #[serde(default, deserialize_with = "crate::bounds::tags")]
    pub tags: Vec<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCardsArgs {
    #[serde(deserialize_with = "crate::bounds::deck_name")]
    pub deck: String,
    #[serde(deserialize_with = "crate::bounds::cards")]
    pub cards: Vec<NewCard>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardPatch {
    pub card_id: i64,
    #[serde(default, deserialize_with = "crate::bounds::opt_field_html")]
    pub front_html: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_field_html")]
    pub back_html: Option<String>,
    #[serde(default, deserialize_with = "crate::bounds::opt_tags")]
    pub tags: Option<Vec<String>>,
    /// Required for shared sources; copy the exact set from list_cards.
    #[serde(default, deserialize_with = "crate::bounds::cards")]
    pub sibling_card_ids: Vec<i64>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCardsArgs {
    #[serde(deserialize_with = "crate::bounds::cards")]
    pub cards: Vec<CardPatch>,
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
    pub before_card_id: Option<i64>,
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
        base_url: String,
    ) -> Self {
        Self {
            services,
            media,
            ext,
            base_url,
            tool_router: Self::tool_router(),
        }
    }

    fn client_id(ctx: &RequestContext<RoleServer>) -> Result<String, McpError> {
        ctx.extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.extensions.get::<crate::oauth::ConnectorClientId>())
            .map(|c| c.0.clone())
            .ok_or_else(|| McpError::invalid_request("unauthenticated", None))
    }

    async fn ask_to_confirm(
        &self,
        user: UserId,
        client: String,
        target: String,
        deck: Option<DeckId>,
        cards: Vec<CardId>,
    ) -> Result<DeleteOutcome, McpError> {
        let token = crate::auth::new_token();
        let hash = crate::auth::hash_token(&token);
        let now = now_ms();
        let preview = run(&self.services, move |s| {
            Ok(s.store()
                .stage_mcp_delete(user, &client, &target, deck, &cards, &hash, now)?)
        })
        .await?;
        Ok(DeleteOutcome {
            deleted: false,
            pending: true,
            confirm_token: Some(token),
            cards_affected: preview.card_ids.len() as u32,
            card_ids: preview.card_ids,
            decks_affected: preview.decks,
            deck_totals: Vec::new(),
            expires_ms: Some(now + 600_000),
            note: Some("Nothing deleted. Describe this exact deletion set, naming every deck in decks_affected, and ask the user to confirm once. After approval, repeat the same arguments with confirm_token. Valid for ten minutes across reconnects.".into()),
        })
    }

    async fn confirmed(
        &self,
        user: UserId,
        client: String,
        target: String,
        token: String,
    ) -> Result<DeleteOutcome, McpError> {
        let hash = crate::auth::hash_token(&token);
        let receipt = run(&self.services, move |s| {
            Ok(s.store()
                .confirm_mcp_delete(user, &client, &target, &hash, now_ms())?)
        })
        .await?;
        crate::media::remove_orphans(
            self.services.clone(),
            self.media.clone(),
            &receipt.orphan_blobs,
        );
        Ok(DeleteOutcome {
            deleted: true,
            pending: false,
            confirm_token: None,
            cards_affected: receipt.card_ids.len() as u32,
            card_ids: receipt.card_ids,
            decks_affected: receipt.decks,
            deck_totals: receipt.deck_totals,
            expires_ms: None,
            note: None,
        })
    }

    async fn question(
        &self,
        user: UserId,
        card: Option<&crate::service::CardFront>,
    ) -> Result<Option<CardFrontOut>, McpError> {
        let Some(card) = card else {
            return Ok(None);
        };
        let id = card.card_id;
        let base = self.base_url.clone();
        run(&self.services, move |s| {
            let row = s.card(user, id)?;
            let front_html = rich_side(row.front_html.as_deref(), &row.front);
            let media = media_out(&s, user, &base, &[&front_html])?;
            Ok(Some(CardFrontOut {
                card_id: id.0,
                front_html,
                media,
            }))
        })
        .await
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
        to study, review, practice, quiz, test, or drill their cards, even \
        cards created moments ago in this chat. Do not quiz from memory: \
        only sessions record progress. Returns the first card's front \
        (ask it aloud and wait for the user's answer), how many cards are \
        due, and grading_mode. Render front_html for display or interpret it for speech; \
        present cloze gaps without revealing answers, and never read raw markup or URLs aloud. \
        Fetch media URLs with the connector Bearer token if needed. Follow grading_mode for the whole \
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
            first_card: self.question(user, session.first_card.as_ref()).await?,
            note: queue_note(session.cards_due, session.held_by_limit),
        }))
    }

    #[tool(
        description = "Record the user's result on a card and get the next \
        one. Grade STRICTLY against the card's back: facts, numbers, doses, \
        and units must match precisely. An imprecise answer is 1 (Again), \
        never 'close enough'; only phrasing may differ. 1=Again (wrong or \
        blank), 2=Hard (incomplete or hesitant), 3=Good (fully correct), \
        4=Easy (instant, perfect). State the exact answer on a miss, then \
        immediately ask next_card's front. A null next_card ends the \
        session. Wrap up briefly.",
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
            next_card: self.question(user, result.next_card.as_ref()).await?,
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
        let base = self.base_url.clone();
        let answer = run(&self.services, move |s| {
            let row = s.card(user, CardId(args.card_id))?;
            let back_html = rich_side(row.back_html.as_deref(), &row.back);
            let media = media_out(&s, user, &base, &[&back_html])?;
            Ok(AnswerOut { back_html, media })
        })
        .await?;
        Ok(Json(answer))
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
        description = "List the user's decks with due and new card counts. \
        A name is a path: Parent::Child is a subdeck of Parent, and each \
        deck is followed by its subdecks. A deck's counts already include \
        its subdecks, so never add rows up. An archived deck keeps its \
        place, with archived: true and zero counts.",
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
                archived: d.archived_at.is_some(),
            })
            .collect();
        Ok(Json(DecksOut { decks: rows }))
    }

    #[tool(
        description = "Create a new (empty) deck. Name it Parent::Child to \
        make a subdeck; parents that do not exist are created too. Not \
        needed before create_cards, which creates its deck.",
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
        description = "Create 1-500 rich sources in a deck (created if missing, with its parents when named Parent::Child), atomically. One call fills one deck. Use front_html/back_html, optional note_type (basic, basic_reversed, basic_typed, cloze), and tags. Preserve media references as /media/{id}; obtain IDs from list_cards. Cloze sources use {{c1::answer}} notation. Keep each question focused. Returns actual generated card IDs and counts.",
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
    ) -> Result<Json<MutationOut>, McpError> {
        let user = authed_user(&ctx)?;
        let result = run(&self.services, move |s| {
            let inputs = args
                .cards
                .into_iter()
                .map(|c| {
                    let note_type = flash_store::notes::NoteType::parse(
                        c.note_type.as_deref().unwrap_or("basic"),
                    )
                    .ok_or_else(|| ServiceError::Invalid("unknown note_type".into()))?;
                    Ok(crate::service::NoteInput {
                        note_type,
                        front_html: c.front_html,
                        back_html: c.back_html,
                        tags: c.tags,
                    })
                })
                .collect::<Result<Vec<_>, ServiceError>>()?;
            let saved = s.create_mcp_notes(user, &args.deck, &inputs, now_ms())?;
            Ok(mutation_out(saved))
        })
        .await?;
        self.account_event(&ctx, user, "cards_created", Some(result.created as i64));
        Ok(Json(result))
    }

    #[tool(
        description = "Update 1-500 card sources atomically. Supply card_id and only fields to change: front_html, back_html, tags. Omitted fields stay intact. Read list_cards first; for shared sources copy sibling_card_ids, and include only one edit per source. Editing a source updates its siblings, retaining their history. Note type is preserved; edits that remove cards are rejected: use delete_cards for confirmed removal.",
        annotations(
            title = "Update cards",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn update_cards(
        &self,
        Parameters(args): Parameters<UpdateCardsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MutationOut>, McpError> {
        let user = authed_user(&ctx)?;
        let result = run(&self.services, move |s| {
            let patches = args
                .cards
                .into_iter()
                .map(|c| crate::service::McpSourcePatch {
                    card_id: CardId(c.card_id),
                    front_html: c.front_html,
                    back_html: c.back_html,
                    tags: c.tags,
                    sibling_card_ids: c.sibling_card_ids.into_iter().map(CardId).collect(),
                })
                .collect::<Vec<_>>();
            let saved = s.update_mcp_sources(user, &patches, now_ms())?;
            Ok(mutation_out(saved))
        })
        .await?;
        self.account_event(&ctx, user, "card_edited", Some(result.updated as i64));
        Ok(Json(result))
    }

    #[tool(
        description = "Delete 1-500 explicit card IDs with one confirmation for the batch. First call returns the exact set and confirm_token, deleting nothing. Ask the user once; after approval repeat the same card_ids with confirm_token. Tokens last ten minutes and survive reconnects. A completed retry returns its original result.",
        annotations(
            title = "Delete cards",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn delete_cards(
        &self,
        Parameters(args): Parameters<DeleteCardsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<DeleteOutcome>, McpError> {
        let user = authed_user(&ctx)?;
        let client = Self::client_id(&ctx)?;
        let mut selected = args.card_ids;
        if selected.is_empty() || selected.len() > 500 || selected.iter().any(|id| *id <= 0) {
            return Err(McpError::invalid_params(
                "supply 1-500 positive card IDs",
                None,
            ));
        }
        selected.sort_unstable();
        if selected.windows(2).any(|w| w[0] == w[1]) {
            return Err(McpError::invalid_params("duplicate card ID", None));
        }
        let target = format!("cards:{}", serde_json::json!(selected));
        match args.confirm_token {
            Some(token) => Ok(Json(self.confirmed(user, client, target, token).await?)),
            None => Ok(Json(
                self.ask_to_confirm(
                    user,
                    client,
                    target,
                    None,
                    selected.into_iter().map(CardId).collect(),
                )
                .await?,
            )),
        }
    }

    #[tool(
        description = "Permanently delete a deck, every subdeck under it (Parent::Child names) and all their cards. First call returns decks_affected, the exact affected card IDs/count and confirm_token without deleting. Name every affected deck to the user, ask once, then repeat the same deck and token. Tokens last ten minutes across reconnects. A changed card or subdeck requires a fresh confirmation. Completed retries return the original result.",
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
        let client = Self::client_id(&ctx)?;
        let target = format!("deck:{}", args.deck.trim());
        if let Some(token) = args.confirm_token {
            return Ok(Json(self.confirmed(user, client, target, token).await?));
        }
        let deck = run(&self.services, move |s| deck_named(&s, user, &args.deck)).await?;
        Ok(Json(
            self.ask_to_confirm(user, client, target, Some(deck), Vec::new())
                .await?,
        ))
    }

    #[tool(
        description = "Read complete cards and editable sources, with formatting and authenticated media URLs. GET media URLs with the same connector Bearer token; do not speak URLs or markup. Source contains cloze answers: use study tools when quizzing. Shared source edits affect sibling_card_ids. Results are newest first; follow next_cursor as before_card_id while has_more. total_count is current, not a frozen snapshot. Default limit 50, max 500; a page of large cards may hold fewer than limit, so trust has_more, not the count.",
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
        let base = self.base_url.clone();
        let out = run(&self.services, move |s| {
            let deck = args
                .deck
                .as_deref()
                .map(|name| deck_named(&s, user, name))
                .transpose()?;
            let limit = args.limit.unwrap_or(50);
            if !(1..=500).contains(&limit) {
                return Err(ServiceError::Invalid("limit must be 1-500".into()));
            }
            let mut rows = s.store().list_cards_cursor(
                user,
                deck,
                args.search.as_deref(),
                limit + 1,
                0,
                args.before_card_id.map(CardId),
            )?;
            let mut has_more = rows.len() > limit as usize;
            rows.truncate(limit as usize);
            let total_count = s.store().count_cards(user, deck, args.search.as_deref())?;
            let mut cards: Vec<CardOut> = Vec::new();
            let mut page_bytes = 0;
            for row in rows {
                let seed = s.editor_seed(user, row.id)?;
                let siblings = s.store().mcp_source_state(user, row.id)?.1;
                let read_only_reason = s.mcp_source_warning(user, &seed, &siblings)?;
                let front_html = rich_side(row.front_html.as_deref(), &row.front);
                let back_html = rich_side(row.back_html.as_deref(), &row.back);
                let media = media_out(
                    &s,
                    user,
                    &base,
                    &[&front_html, &back_html, &seed.front_html, &seed.back_html],
                )?;
                let card = CardOut {
                    card_id: row.id.0,
                    deck_id: row.deck_id.0,
                    ord: row.ord,
                    cloze_index: row.cloze_index,
                    type_answer: row.type_answer,
                    front_html,
                    back_html,
                    media,
                    source: SourceOut {
                        read_only_reason,
                        note_id: seed.note_id.map(|id| id.0),
                        note_type: seed.note_type.as_str().into(),
                        front_html: seed.front_html,
                        back_html: seed.back_html,
                        tags: seed.tags,
                        sibling_card_ids: siblings.iter().map(|id| id.0).collect(),
                    },
                };
                // A page is bounded by size as well as by count: rich
                // imported cards run to kilobytes each, and a client drops
                // a tool result that is too large. The cursor makes a
                // short page harmless. One card always fits.
                page_bytes += serde_json::to_vec(&card).map_or(0, |v| v.len());
                if page_bytes > LIST_PAGE_BYTES && !cards.is_empty() {
                    has_more = true;
                    break;
                }
                cards.push(card);
            }
            let next_cursor = if has_more {
                cards.last().map(|c| c.card_id)
            } else {
                None
            };
            Ok(CardsOut {
                cards,
                total_count,
                next_cursor,
                has_more,
            })
        })
        .await?;
        Ok(Json(out))
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
        description = "Rename a deck or move it into another deck. Use when \
        the user asks to rename a deck, nest it under another, or pull it \
        out to the top level. new_name is the full path: Pharm::Cardio \
        moves Cardio into Pharm (created if missing). Its subdecks, cards \
        and review history follow. Refused if a deck already has that \
        name (decks are never merged) or the target is inside the deck \
        itself.",
        annotations(
            title = "Rename or move deck",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn rename_deck(
        &self,
        Parameters(args): Parameters<RenameDeckArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<RenamedOut>, McpError> {
        let user = authed_user(&ctx)?;
        let out = run(&self.services, move |s| {
            let id = deck_named(&s, user, &args.deck)?;
            s.rename_deck(user, id, &args.new_name)?;
            let detail = s.deck_detail(user, id, now_ms())?;
            Ok(RenamedOut {
                renamed: true,
                name: detail.deck.name,
                decks_renamed: detail.subdeck_count + 1,
            })
        })
        .await?;
        Ok(Json(out))
    }

    #[tool(
        description = "Archive a deck the user is done with (an exam passed): \
        every card, its media and its review history stay, but the deck \
        leaves the study queue and the due counts, and start_study_session \
        on it finds nothing. Nothing is deleted, so no confirmation is \
        needed; unarchive_deck reverses it. Its subdecks are archived \
        with it. Cards can still be added to an archived deck by name.",
        annotations(
            title = "Archive deck",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn archive_deck(
        &self,
        Parameters(args): Parameters<DeckNameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SavedOut>, McpError> {
        let user = authed_user(&ctx)?;
        run(&self.services, move |s| {
            let id = deck_named(&s, user, &args.deck)?;
            s.archive_deck(user, id, now_ms())
        })
        .await?;
        Ok(Json(SavedOut { saved: true }))
    }

    #[tool(
        description = "Put an archived deck back into study. Its cards come \
        back with the review history they had, so whatever came due while \
        it was archived is due now, within the daily limits. Its subdecks \
        come back with it, and so do the decks it sits in.",
        annotations(
            title = "Unarchive deck",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn unarchive_deck(
        &self,
        Parameters(args): Parameters<DeckNameArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SavedOut>, McpError> {
        let user = authed_user(&ctx)?;
        run(&self.services, move |s| {
            let id = deck_named(&s, user, &args.deck)?;
            s.unarchive_deck(user, id)
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
            Some("Spaced-repetition study and rich flashcard management.".to_string());
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
