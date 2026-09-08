//! A password-reset link is one way into an account, and a credential
//! change ends every way in that predates it: the unused links die with
//! a password change, a first password, a password removal, and with
//! the reset that was used. A link saved from a mailbox someone briefly
//! held is not a second chance after the owner reacted.

mod common;

use common::*;
use flash_server::auth::{hash_token, new_token};
use flash_server::flows::{account, reset};
use flash_server::service::now_ms;
use flash_store::Store;

fn pending_reset(store: &Store, user: flash_core::UserId) -> String {
    let token = new_token();
    let now = now_ms();
    store
        .create_password_reset(&hash_token(&token), user, now, now + 3_600_000)
        .unwrap();
    hash_token(&token)
}

fn live(store: &Store, hash: &str) -> bool {
    store
        .lookup_password_reset(hash, now_ms())
        .unwrap()
        .is_some()
}

#[test]
fn a_password_change_kills_the_unused_reset_links() {
    let t = AppBuilder::new("reset-hygiene-change").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    account::set_password(&t.store, user, "$argon2id$old").unwrap();
    let a = pending_reset(&t.store, user);
    let b = pending_reset(&t.store, user);
    assert!(live(&t.store, &a) && live(&t.store, &b));

    let keep = account::KeepSession {
        web_session_hash: None,
        api_token_id: None,
    };
    account::change_password(&t.store, user, "$argon2id$new", &keep, now_ms()).unwrap();
    assert!(!live(&t.store, &a) && !live(&t.store, &b));
}

#[test]
fn a_used_reset_kills_the_others_and_a_first_password_kills_them_too() {
    let t = AppBuilder::new("reset-hygiene-confirm").build();
    let user = member(&t.store, "Ada", "ada@example.com");
    let used = pending_reset(&t.store, user);
    let saved = pending_reset(&t.store, user);
    assert_eq!(
        reset::confirm(&t.store, &used, "$argon2id$new", now_ms()).unwrap(),
        Some(user)
    );
    assert!(!live(&t.store, &saved), "the other link is dead with it");

    let other = member(&t.store, "Bob", "bob@example.com");
    let link = pending_reset(&t.store, other);
    account::set_password(&t.store, other, "$argon2id$first").unwrap();
    assert!(!live(&t.store, &link));
}
