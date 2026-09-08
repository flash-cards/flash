//! Every limit on what a request may carry, in one place, and the serde
//! hooks that apply them at the edge. A request struct's string or list
//! field names its bound with `#[serde(deserialize_with = "bounds::…")]`,
//! and `bounded_input_tests` fails the build for any such field that
//! does not, so a handler cannot receive an unbounded value by omission.
//! The limits that depend on stored state (decks per account, media
//! objects per account) live here too and are enforced by the service.
//!
//! The numbers are generous for people and tight for scripts: a deck
//! name of two hundred bytes, fifty tags on a card, five hundred cards
//! in one call. Beyond them a request is refused with a sentence, not
//! parsed and then regretted.

use serde::de::{Deserialize, Deserializer, Error};

/// Per-field byte limits.
pub const DECK_NAME: usize = 200;
pub const DECK_DESCRIPTION: usize = 2_000;
/// The store refuses longer tags and larger lists on every write path;
/// the edge refuses them first so the sentence names the field.
pub const TAG: usize = flash_store::MAX_TAG_LEN;
pub const DISPLAY_NAME: usize = 120;
pub const EMAIL: usize = 254;
/// Long enough for any passphrase; short enough that hashing it is
/// bounded work.
pub const PASSWORD: usize = 1_024;
pub const DEVICE_LABEL: usize = 120;
/// Opaque tokens and ids the server minted: ceremony ids, refresh and
/// import tokens, client ids, authorization codes.
pub const TOKEN: usize = 256;
/// A typed answer on the study screen, matched against a card side.
pub const TYPED_ANSWER: usize = flash_core::MAX_SIDE_LEN;
/// A plain card side (front or back) from the API or MCP.
pub const CARD_SIDE: usize = flash_core::MAX_SIDE_LEN;
/// One editor field of HTML, before sanitizing.
pub const FIELD_HTML: usize = flash_store::notes::MAX_FIELD_HTML;
/// A search term against card text. Anything longer than a card side
/// cannot match; anything longer than this makes LIKE expensive.
pub const SEARCH: usize = 100;
pub const TIMEZONE: usize = 64;
/// Enum-shaped strings: a theme, a grading mode, a note type, a colour
/// choice, a decision, a saved-flag.
pub const KEYWORD: usize = 32;
/// A local path to return to after sign-in.
pub const NEXT_PATH: usize = 512;
/// OAuth request parameters (RFC 6749 leaves these unbounded; a real
/// client's are a few hundred bytes).
pub const OAUTH_STATE: usize = 512;
pub const OAUTH_SCOPE: usize = 256;
pub const OAUTH_RESOURCE: usize = 500;
/// RFC 7636: 43 to 128 characters of unreserved ASCII.
pub const CODE_CHALLENGE: usize = 128;
pub const CODE_VERIFIER: usize = 128;
pub const REDIRECT_URI: usize = 2_048;
pub const CLIENT_NAME: usize = 200;
/// A comma-separated tag list typed into a web form.
pub const TAG_LIST_TEXT: usize = TAGS_PER_CARD * (TAG + 1);
/// A signed token from an identity or store provider (a JWT, a JWS
/// transaction): three base64url segments, a few kilobytes at most.
pub const SIGNED_TOKEN: usize = 8 * 1024;
/// An opaque token a provider or the server minted and the client
/// echoes back (a purchase token, a push token, a referral code).
pub const OPAQUE: usize = 1_024;
/// A title of something a person names (a shared deck).
pub const TITLE: usize = 200;
/// Free text a person writes about something (a report reason).
pub const REASON: usize = 2_000;
/// A captcha response token (Turnstile documents up to 2,048 characters).
pub const CAPTCHA_TOKEN: usize = 2_048;
/// Path plus query of any request, enforced by `middleware::uri_length`
/// before routing: the bound on every `Path<String>` and `RawQuery`.
pub const URI: usize = 2_048;
/// A filename a client attaches to an upload, before the store shortens
/// it to what it keeps.
pub const FILENAME: usize = 255;

/// Per-request count limits.
pub const TAGS_PER_CARD: usize = flash_store::MAX_TAGS_PER_CARD;
pub const CARDS_PER_CALL: usize = 500;
pub const REDIRECT_URIS_PER_CLIENT: usize = 10;

/// Per-account limits on stored state, enforced where the row is
/// created: decks in the store's `create_deck`, so an import naming a
/// new deck meets the same cap as a form; media in the server's ingest
/// paths, the only writers of a blob.
pub const DECKS_PER_USER: u32 = flash_store::MAX_DECKS_PER_USER;
pub const MEDIA_OBJECTS_PER_USER: u64 = 20_000;

fn too_long<E: Error>(what: &str, max: usize) -> E {
    E::custom(format!("{what} is longer than {max} bytes"))
}

fn too_many<E: Error>(what: &str, max: usize) -> E {
    E::custom(format!("more than {max} {what}"))
}

fn text<'de, D: Deserializer<'de>>(d: D, what: &str, max: usize) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.len() > max {
        return Err(too_long(what, max));
    }
    Ok(s)
}

fn opt_text<'de, D: Deserializer<'de>>(
    d: D,
    what: &str,
    max: usize,
) -> Result<Option<String>, D::Error> {
    match Option::<String>::deserialize(d)? {
        Some(s) if s.len() > max => Err(too_long(what, max)),
        other => Ok(other),
    }
}

fn texts<'de, D: Deserializer<'de>>(
    d: D,
    what: &str,
    count: usize,
    each: usize,
) -> Result<Vec<String>, D::Error> {
    let v = Vec::<String>::deserialize(d)?;
    if v.len() > count {
        return Err(too_many(what, count));
    }
    if let Some(long) = v.iter().find(|s| s.len() > each) {
        let _ = long;
        return Err(too_long(what, each));
    }
    Ok(v)
}

// One named hook per kind of field. The names are what the request
// structs cite and what the inventory test looks for.

macro_rules! text_hook {
    ($name:ident, $what:literal, $max:expr) => {
        pub fn $name<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
            text(d, $what, $max)
        }
    };
}

macro_rules! opt_text_hook {
    ($name:ident, $what:literal, $max:expr) => {
        pub fn $name<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
            opt_text(d, $what, $max)
        }
    };
}

text_hook!(deck_name, "deck name", DECK_NAME);
text_hook!(deck_description, "deck description", DECK_DESCRIPTION);
text_hook!(tag, "tag", TAG);
text_hook!(display_name, "display name", DISPLAY_NAME);
text_hook!(email, "email address", EMAIL);
text_hook!(password, "password", PASSWORD);
text_hook!(device_label, "device label", DEVICE_LABEL);
text_hook!(token, "token", TOKEN);
text_hook!(card_side, "card side", CARD_SIDE);
text_hook!(field_html, "field", FIELD_HTML);
text_hook!(keyword, "value", KEYWORD);
text_hook!(tag_list_text, "tag list", TAG_LIST_TEXT);
text_hook!(redirect_uri, "redirect_uri", REDIRECT_URI);
text_hook!(client_name, "client_name", CLIENT_NAME);
text_hook!(signed_token, "token", SIGNED_TOKEN);
text_hook!(opaque, "token", OPAQUE);
text_hook!(title, "title", TITLE);
text_hook!(reason, "reason", REASON);
text_hook!(description, "description", DECK_DESCRIPTION);
text_hook!(captcha_token, "captcha token", CAPTCHA_TOKEN);
text_hook!(next_path, "next", NEXT_PATH);

opt_text_hook!(opt_deck_name, "deck name", DECK_NAME);
opt_text_hook!(opt_deck_description, "deck description", DECK_DESCRIPTION);
opt_text_hook!(opt_tag, "tag", TAG);
opt_text_hook!(opt_password, "password", PASSWORD);
opt_text_hook!(opt_token, "token", TOKEN);
opt_text_hook!(opt_typed_answer, "typed answer", TYPED_ANSWER);
opt_text_hook!(opt_search, "search", SEARCH);
opt_text_hook!(opt_timezone, "timezone", TIMEZONE);
opt_text_hook!(opt_keyword, "value", KEYWORD);
opt_text_hook!(opt_next_path, "next", NEXT_PATH);
opt_text_hook!(opt_oauth_state, "state", OAUTH_STATE);
opt_text_hook!(opt_oauth_scope, "scope", OAUTH_SCOPE);
opt_text_hook!(opt_oauth_resource, "resource", OAUTH_RESOURCE);
opt_text_hook!(opt_code_challenge, "code_challenge", CODE_CHALLENGE);
opt_text_hook!(opt_code_verifier, "code_verifier", CODE_VERIFIER);
opt_text_hook!(opt_redirect_uri, "redirect_uri", REDIRECT_URI);
opt_text_hook!(opt_client_name, "client_name", CLIENT_NAME);
opt_text_hook!(opt_signed_token, "token", SIGNED_TOKEN);
opt_text_hook!(opt_opaque, "token", OPAQUE);
opt_text_hook!(opt_display_name, "name", DISPLAY_NAME);
opt_text_hook!(opt_email, "email address", EMAIL);

/// A card's tags: at most `TAGS_PER_CARD`, each at most `TAG` bytes.
pub fn tags<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    texts(d, "tags", TAGS_PER_CARD, TAG)
}

pub fn opt_tags<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    match Option::<Vec<String>>::deserialize(d)? {
        None => Ok(None),
        Some(v) => {
            if v.len() > TAGS_PER_CARD {
                return Err(too_many("tags", TAGS_PER_CARD));
            }
            if v.iter().any(|s| s.len() > TAG) {
                return Err(too_long("tag", TAG));
            }
            Ok(Some(v))
        }
    }
}

/// A client registration's redirect URIs.
pub fn redirect_uris<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    texts(d, "redirect_uris", REDIRECT_URIS_PER_CLIENT, REDIRECT_URI)
}

/// A list of at most `CARDS_PER_CALL` items of any deserializable type
/// (each item bounds its own fields).
pub fn cards<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    let v = Vec::<T>::deserialize(d)?;
    if v.len() > CARDS_PER_CALL {
        return Err(too_many("cards in one call", CARDS_PER_CALL));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Probe {
        #[serde(deserialize_with = "deck_name")]
        name: String,
        #[serde(default, deserialize_with = "tags")]
        tags: Vec<String>,
        #[serde(default, deserialize_with = "opt_search")]
        q: Option<String>,
    }

    #[test]
    fn a_field_past_its_bound_is_refused_with_a_sentence() {
        let long = "x".repeat(DECK_NAME + 1);
        let err = serde_json::from_str::<Probe>(&format!(r#"{{"name":"{long}"}}"#)).unwrap_err();
        assert!(err
            .to_string()
            .contains("deck name is longer than 200 bytes"));

        let many: Vec<String> = (0..=TAGS_PER_CARD).map(|i| i.to_string()).collect();
        let body = serde_json::json!({"name": "ok", "tags": many}).to_string();
        let err = serde_json::from_str::<Probe>(&body).unwrap_err();
        assert!(err.to_string().contains("more than 50 tags"));

        let ok: Probe = serde_json::from_str(r#"{"name":"Pharm","tags":["a"],"q":"x"}"#).unwrap();
        assert_eq!(ok.name, "Pharm");
        assert_eq!(ok.tags, ["a"]);
        assert_eq!(ok.q.as_deref(), Some("x"));
    }
}
