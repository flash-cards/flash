//! What a hosted build plugs into the open server. The open server is a
//! complete product on its own — accounts, decks, study, import, MCP —
//! and `build_app` composes it from core routers plus whatever the
//! extension returns for each band. Every method has a no-op default;
//! `CoreOnly` implements nothing and is what the open binary installs.

use std::any::Any;
use std::sync::Arc;

use axum::http::HeaderValue;
use axum::Router;
use flash_core::{DeckId, UserId};
use flash_store::Store;

use crate::captcha::CaptchaVerifier;
use crate::flows::account::DeleteRefusal;
use crate::service::Services;
use crate::state::AppState;

/// Which JSON view of the account the extension is adding fields to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountView {
    /// `Me`: what every sign-in and `/me` return.
    Me,
    /// `Settings`: everything the settings screen renders.
    Settings,
}

/// Extra top-level JSON fields, flattened into a core object.
pub type JsonFields = serde_json::Map<String, serde_json::Value>;

/// Which of the product's surfaces an account action came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Web,
    Mobile,
    Mcp,
    Api,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Web => "web",
            Surface::Mobile => "mobile",
            Surface::Mcp => "mcp",
            Surface::Api => "api",
        }
    }
}

/// Which part of the sidebar `ServerExtension::nav_html` is filling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavSection {
    /// The study group at the top, after "Stats".
    Primary,
    /// The Account group, after the admin-only "Users" entry.
    Account,
}

/// The places in the core's pages that are the extension's to fill. Each
/// is rendered to a String the page emits with `|safe`; an extension
/// that has nothing for a slot returns "". The store-backed ones (usage,
/// settings, sharing) are rendered inside the page's blocking closure.
pub enum Slot<'a> {
    /// login.html, above "Sign in with passkey": external sign-in buttons,
    /// plus the error line when `err` is one of the extension's codes.
    LoginProviders {
        next: &'a str,
        err: Option<&'a str>,
        /// Channel attribution to carry into a new account (signup page).
        ref_code: Option<&'a str>,
    },
    /// The challenge widget on the email-sending public forms.
    Captcha,
    /// decks / deck / import: the over-limit banner; `limit_hit` after a
    /// blocked add, so the banner says why.
    UsageBanner { user: UserId, limit_hit: bool },
    /// decks / import: the quiet running count.
    UsageCount { user: UserId },
    /// settings: the plan-and-billing card, replacing the core's plain
    /// "Your cards" card when non-empty. `query` is the raw query string
    /// the extension's own redirects carry flags in.
    SettingsPlan { user: UserId, query: &'a str },
    /// settings: connected external accounts.
    SettingsProviders { user: UserId, query: &'a str },
    /// settings, delete card header: the extension's refusal badge.
    SettingsDeleteBadge { query: &'a str },
    /// settings, delete card: what deleting does to a subscription.
    SettingsDeleteNote { user: UserId },
    /// deck settings: the sharing card.
    DeckSharing {
        user: UserId,
        deck: DeckId,
        query: &'a str,
    },
    /// admin users: the plan cell of one row; the column exists only
    /// when some row renders one.
    AdminUserPlan { user: UserId },
    /// The `<head>` of a public page the core renders whole (connect):
    /// the extension's own tags, such as a script that reports a visitor
    /// once they interact with the page.
    PublicHead,
}

/// Where account security consults the extension: the core never leaves
/// an account without a way in and never deletes the last admin; the
/// extension adds its own login methods and its own reasons to refuse a
/// deletion (a subscription that could still bill, say).
/// What the extension's signup loop attached to an invite: the mailbox
/// the verification link was mailed to (the account is fixed to it, and
/// possession of the link proves it) and the channel that brought the
/// signup. Admin-created invites carry none of this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignupInvite {
    pub email: String,
    pub ref_code: Option<String>,
}

pub trait AccountHooks: Send + Sync {
    /// The signup details behind a valid invite; None for admin invites
    /// (the open server's only kind).
    fn signup_invite(
        &self,
        _store: &Store,
        _invite_token_hash: &str,
    ) -> flash_store::Result<Option<SignupInvite>> {
        Ok(None)
    }

    /// An account was just created from an invite; `signup` is what
    /// `signup_invite` returned for it, `invite_token_hash` names the
    /// invite it came from. The extension records what it knows (a
    /// verified address, channel attribution, marks the invite carried).
    fn user_enrolled(
        &self,
        _store: &Store,
        _user: UserId,
        _signup: Option<&SignupInvite>,
        _invite_token_hash: &str,
        _now_ms: i64,
    ) -> flash_store::Result<()> {
        Ok(())
    }

    /// An external login that keeps the account reachable without a
    /// password or passkey, so either may be removed.
    fn has_external_login(&self, _store: &Store, _user: UserId) -> flash_store::Result<bool> {
        Ok(false)
    }

    /// Runs after the last-admin check and before the purge. `Err` on
    /// the inner result aborts with nothing deleted; `Ok` may have had
    /// side effects of its own (a subscription cancelled, a grant
    /// revoked).
    fn before_delete(
        &self,
        _state: &AppState,
        _user: UserId,
        _now_ms: i64,
    ) -> crate::service::Result<std::result::Result<(), DeleteRefusal>> {
        Ok(Ok(()))
    }
}

pub trait ServerExtension: AccountHooks + Send + Sync + 'static {
    /// For the extension's own handlers to reach their own state through
    /// `AppState.ext` (`as_any().downcast_ref()`).
    fn as_any(&self) -> &dyn Any;

    // ---- routers, one per band in build_app ----

    /// The crawler-facing site root: `/`, `/robots.txt` and any sitemap
    /// or marketing pages the deployment wants to own. None keeps the
    /// core's: dashboard-or-login at `/`, robots.txt disallowing all.
    fn site_routes(&self, _state: &AppState) -> Option<Router<AppState>> {
        None
    }

    /// Browser routes under the same-origin mutation guard.
    fn browser_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
    }

    /// Browser routes under the same-origin guard *and* the per-IP auth
    /// rate limit: credential ceremonies, and anything else a stranger
    /// can hit repeatedly.
    fn auth_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
    }

    /// Outside the CSRF guard: signature-authenticated webhooks and
    /// cross-site form posts from identity providers. Apply any rate
    /// limit inside the returned router.
    fn protocol_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
    }

    /// Nested under /api/v1 by the core, so the API limiter and the
    /// no-store header apply.
    fn api_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
    }

    /// Site-root JSON the platforms fetch (app association files).
    fn well_known_routes(&self, _state: &AppState) -> Router<AppState> {
        Router::new()
    }

    // ---- fragments the core's pages render ----

    /// Sidebar items for one section of base.html: `Primary` renders after
    /// "Stats", `Account` after "Users". `active` is the page's nav key;
    /// admin-only items check `is_admin`. Pure: no store.
    fn nav_html(&self, _section: NavSection, _active: &str, _is_admin: bool) -> String {
        String::new()
    }

    /// One slot's HTML. Store-backed slots are rendered on a blocking
    /// thread, inside the page's own closure.
    fn render(&self, _state: &AppState, _slot: Slot<'_>) -> crate::service::Result<String> {
        Ok(String::new())
    }

    /// Where "Create an account" points; None hides it.
    fn signup_path(&self, _state: &AppState) -> Option<String> {
        None
    }

    // ---- JSON the API carries for the extension ----

    /// Fields flattened into the `Me` / `Settings` objects (plan, usage,
    /// billing, connected providers). Runs on a blocking thread.
    fn account_json(
        &self,
        _state: &AppState,
        _s: &Services,
        _user: UserId,
        _view: AccountView,
    ) -> crate::service::Result<JsonFields> {
        Ok(JsonFields::new())
    }

    /// Fields flattened into each row of the admin users list (the
    /// plan). Runs on a blocking thread.
    fn admin_user_json(&self, _s: &Services, _user: UserId) -> crate::service::Result<JsonFields> {
        Ok(JsonFields::new())
    }

    /// `/meta` `features` the extension turns on; the core seeds every
    /// key it knows as false, so an app never sees a key vanish.
    fn features(&self, _state: &AppState) -> JsonFields {
        JsonFields::new()
    }

    /// Extra top-level `/meta` fields (store product ids and the like).
    fn meta_extra(&self, _state: &AppState) -> JsonFields {
        JsonFields::new()
    }

    /// An anonymous product-funnel counter: event name and channel code
    /// only, never a user id. Fire-and-forget.
    fn count_event(&self, _s: &Services, _event: &'static str, _ref_code: Option<&str>) {}

    /// A signed-in account did something the deployment may want to count
    /// per account: the event, the surface it came from, an optional
    /// short label (a client name, a login method, a platform) and an
    /// optional magnitude (cards imported). Fire-and-forget; never a
    /// request address or user agent.
    fn account_event(
        &self,
        _s: &Services,
        _user: UserId,
        _event: &'static str,
        _surface: Surface,
        _detail: Option<&str>,
        _amount: Option<i64>,
    ) {
    }

    // ---- the challenge on email-sending public forms ----

    /// Verifies the form's challenge token; None means no challenge.
    fn captcha_verifier(&self, _state: &AppState) -> Option<Arc<dyn CaptchaVerifier>> {
        None
    }

    /// The Content-Security-Policy a page carrying the challenge widget
    /// needs (its script and frame origins).
    fn captcha_csp(&self, _state: &AppState) -> Option<HeaderValue> {
        None
    }
}

/// The open server: nothing plugged in.
pub struct CoreOnly;

impl AccountHooks for CoreOnly {}

impl ServerExtension for CoreOnly {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
